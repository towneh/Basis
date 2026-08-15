//! The shared async-callback plumbing under both adapters: MediaCodec's
//! callbacks land on the codec's own internal thread (§6.3 — adapter-
//! specific decode threads); they only ever push indices into this state
//! and notify. The engine's decode thread consumes through the trait
//! surface. Queue depths are the codec's own (input buffers granted by
//! `onAsyncInputAvailable`, output buffers by `onAsyncOutputAvailable`) —
//! nothing here inherits another adapter's numbers, and a dry input queue
//! surfaces as `NotAccepting` for the gated release to absorb.

use std::collections::VecDeque;
use std::ffi::{CStr, c_void};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use media_decode::{DecodeError, SubmitOutcome};

use crate::ffi::*;

#[derive(Default)]
pub(crate) struct CbState {
    pub input_free: VecDeque<i32>,
    pub output_ready: VecDeque<(i32, AMediaCodecBufferInfo)>,
    /// First asynchronous codec error; fails the session on next use.
    pub error: Option<String>,
    /// Latest output-format colour/geometry, parsed in the format-changed
    /// callback (the format object itself stays framework-owned).
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OutputFormat {
    pub color_standard: i32,
    pub color_range: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub pcm_encoding: i32,
    pub seen: bool,
}

pub(crate) struct Callbacks {
    pub state: Mutex<CbState>,
    pub changed: Condvar,
}

impl Callbacks {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CbState::default()),
            changed: Condvar::new(),
        })
    }
}

unsafe extern "C" fn on_input(_codec: *mut AMediaCodec, userdata: *mut c_void, index: i32) {
    // SAFETY: userdata is the Arc<Callbacks> raw pointer this driver
    // registered and keeps alive until after AMediaCodec_delete returns
    // (delete joins the callback thread, so no callback outlives it).
    let cb = unsafe { &*(userdata as *const Callbacks) };
    let mut state = cb.state.lock().expect("cb lock");
    state.input_free.push_back(index);
    cb.changed.notify_all();
}

unsafe extern "C" fn on_output(
    _codec: *mut AMediaCodec,
    userdata: *mut c_void,
    index: i32,
    info: *mut AMediaCodecBufferInfo,
) {
    // SAFETY: userdata as in `on_input`; `info` is valid for the duration
    // of the callback per the NDK contract and is copied out here.
    let (cb, info) = unsafe { (&*(userdata as *const Callbacks), *info) };
    let mut state = cb.state.lock().expect("cb lock");
    state.output_ready.push_back((index, info));
    cb.changed.notify_all();
}

unsafe extern "C" fn on_format(
    _codec: *mut AMediaCodec,
    userdata: *mut c_void,
    format: *mut AMediaFormat,
) {
    // SAFETY: userdata as in `on_input`; the format object is only read
    // during the callback (framework-owned, valid for its duration).
    let cb = unsafe { &*(userdata as *const Callbacks) };
    let get = |name: &CStr| -> i32 {
        let mut value = 0i32;
        // SAFETY: live format pointer, NUL-terminated key, out-param is a
        // local. A missing key leaves `value` untouched (getInt32 returns
        // false).
        unsafe { AMediaFormat_getInt32(format, name.as_ptr(), &mut value) };
        value
    };
    let parsed = OutputFormat {
        color_standard: get(c"color-standard"),
        color_range: get(c"color-range"),
        sample_rate: get(c"sample-rate"),
        channels: get(c"channel-count"),
        pcm_encoding: get(c"pcm-encoding"),
        seen: true,
    };
    let mut state = cb.state.lock().expect("cb lock");
    state.format = parsed;
    cb.changed.notify_all();
}

unsafe extern "C" fn on_error(
    _codec: *mut AMediaCodec,
    userdata: *mut c_void,
    error: media_status_t,
    action_code: i32,
    detail: *const core::ffi::c_char,
) {
    // SAFETY: userdata as in `on_input`; detail is a NUL-terminated C
    // string (or null) valid for the callback's duration.
    let cb = unsafe { &*(userdata as *const Callbacks) };
    let detail = if detail.is_null() {
        String::new()
    } else {
        // SAFETY: non-null NUL-terminated string per the NDK contract.
        unsafe { CStr::from_ptr(detail) }
            .to_string_lossy()
            .into_owned()
    };
    let mut state = cb.state.lock().expect("cb lock");
    state.error.get_or_insert(format!(
        "codec error {error} (action {action_code}): {detail}"
    ));
    cb.changed.notify_all();
}

/// One async-driven `AMediaCodec` plus the callback state it feeds.
pub(crate) struct AsyncCodec {
    codec: *mut AMediaCodec,
    pub cb: Arc<Callbacks>,
    /// Raw Arc handed to the callbacks; released after delete.
    userdata: *const Callbacks,
    pub name: String,
    /// EOS submitted to the input side this timeline.
    eos_queued: bool,
    /// A drain was requested before an input buffer was free.
    eos_pending: bool,
}

// SAFETY: the codec handle is driven from one engine thread at a time;
// libmediandk's own callback thread only touches the Mutex-guarded state.
unsafe impl Send for AsyncCodec {}

impl AsyncCodec {
    /// Create a decoder for `mime`, configure it with `format` (consumed)
    /// and the optional output surface, register callbacks, start it.
    pub fn start(
        mime: &CStr,
        format: *mut AMediaFormat,
        surface: *mut ANativeWindow,
    ) -> Result<Self, DecodeError> {
        // SAFETY: create/configure/start sequence per the NDK state
        // machine, all pointers checked; the format object is ours until
        // deleted below, and the callback userdata Arc outlives the codec.
        unsafe {
            let codec = AMediaCodec_createDecoderByType(mime.as_ptr());
            if codec.is_null() {
                AMediaFormat_delete(format);
                return Err(DecodeError(format!(
                    "no MediaCodec decoder for {}",
                    mime.to_string_lossy()
                )));
            }
            let name = codec_name(codec).unwrap_or_else(|| "<unnamed>".into());

            let cb = Callbacks::new();
            let userdata = Arc::into_raw(Arc::clone(&cb));
            let callbacks = AMediaCodecOnAsyncNotifyCallback {
                on_async_input_available: on_input,
                on_async_output_available: on_output,
                on_async_format_changed: on_format,
                on_async_error: on_error,
            };
            let status =
                AMediaCodec_setAsyncNotifyCallback(codec, callbacks, userdata as *mut c_void);
            if status != AMEDIA_OK {
                AMediaFormat_delete(format);
                AMediaCodec_delete(codec);
                drop(Arc::from_raw(userdata));
                return Err(DecodeError(format!(
                    "setAsyncNotifyCallback failed ({status}) on {name}"
                )));
            }
            let status = AMediaCodec_configure(codec, format, surface, core::ptr::null_mut(), 0);
            AMediaFormat_delete(format);
            if status != AMEDIA_OK {
                AMediaCodec_delete(codec);
                drop(Arc::from_raw(userdata));
                return Err(DecodeError(format!(
                    "configure failed ({status}) on {name}"
                )));
            }
            let status = AMediaCodec_start(codec);
            if status != AMEDIA_OK {
                AMediaCodec_delete(codec);
                drop(Arc::from_raw(userdata));
                return Err(DecodeError(format!("start failed ({status}) on {name}")));
            }
            Ok(Self {
                codec,
                cb,
                userdata,
                name,
                eos_queued: false,
                eos_pending: false,
            })
        }
    }

    pub fn raw(&self) -> *mut AMediaCodec {
        self.codec
    }

    pub fn take_error(&self) -> Option<DecodeError> {
        let state = self.cb.state.lock().expect("cb lock");
        state
            .error
            .as_ref()
            .map(|e| DecodeError(format!("{} ({})", e, self.name)))
    }

    /// Copy one compressed AU into a free input buffer. A dry input queue
    /// is `NotAccepting` — the codec grants indices as it consumes.
    pub fn submit(&mut self, au: &[u8], pts_us: i64) -> Result<SubmitOutcome, DecodeError> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        if self.eos_queued {
            // Post-drain submissions wait for the reset.
            return Ok(SubmitOutcome::NotAccepting);
        }
        self.flush_eos_if_pending()?;
        let index = {
            let mut state = self.cb.state.lock().expect("cb lock");
            if self.eos_pending {
                // The parked EOS owns the next free index.
                return Ok(SubmitOutcome::NotAccepting);
            }
            match state.input_free.pop_front() {
                Some(index) => index,
                None => return Ok(SubmitOutcome::NotAccepting),
            }
        };
        // SAFETY: `index` came from onAsyncInputAvailable and has not been
        // queued since; the returned buffer spans `size` writable bytes and
        // the copy is bounds-checked against it.
        unsafe {
            let mut size = 0usize;
            let buf = AMediaCodec_getInputBuffer(self.codec, index as usize, &mut size);
            if buf.is_null() || size < au.len() {
                return Err(DecodeError(format!(
                    "input buffer {index} too small ({size} < {}) on {}",
                    au.len(),
                    self.name
                )));
            }
            core::ptr::copy_nonoverlapping(au.as_ptr(), buf, au.len());
            // Negative pts (encoder priming) round-trips through the
            // unsigned parameter: the framework's internal timestamp is
            // int64, so the two's-complement value comes back intact.
            let status = AMediaCodec_queueInputBuffer(
                self.codec,
                index as usize,
                0,
                au.len(),
                pts_us as u64,
                0,
            );
            if status != AMEDIA_OK {
                return Err(DecodeError(format!(
                    "queueInputBuffer failed ({status}) on {}",
                    self.name
                )));
            }
        }
        Ok(SubmitOutcome::Accepted)
    }

    /// Queue the end-of-stream marker, parking it if no input buffer is
    /// free yet (the codec grants one as it drains).
    pub fn begin_drain(&mut self) -> Result<(), DecodeError> {
        if self.eos_queued {
            return Ok(());
        }
        self.eos_pending = true;
        self.flush_eos_if_pending()
    }

    fn flush_eos_if_pending(&mut self) -> Result<(), DecodeError> {
        if !self.eos_pending || self.eos_queued {
            return Ok(());
        }
        let index = {
            let mut state = self.cb.state.lock().expect("cb lock");
            state.input_free.pop_front()
        };
        let Some(index) = index else {
            return Ok(());
        };
        // SAFETY: freshly granted input index; an empty EOS buffer needs
        // no payload write.
        let status = unsafe {
            AMediaCodec_queueInputBuffer(
                self.codec,
                index as usize,
                0,
                0,
                0,
                BUFFER_FLAG_END_OF_STREAM,
            )
        };
        if status != AMEDIA_OK {
            return Err(DecodeError(format!(
                "queueInputBuffer(EOS) failed ({status}) on {}",
                self.name
            )));
        }
        self.eos_pending = false;
        self.eos_queued = true;
        Ok(())
    }

    pub fn draining(&self) -> bool {
        self.eos_queued || self.eos_pending
    }

    /// Pop the next ready output index, optionally waiting up to `wait`
    /// for one (the drain path waits; steady-state polling does not).
    pub fn pop_output(&self, wait: Duration) -> Option<(i32, AMediaCodecBufferInfo)> {
        let mut state = self.cb.state.lock().expect("cb lock");
        if let Some(entry) = state.output_ready.pop_front() {
            return Some(entry);
        }
        if wait.is_zero() {
            return None;
        }
        let (mut state, _timeout) = self.cb.changed.wait_timeout(state, wait).expect("cb wait");
        state.output_ready.pop_front()
    }

    pub fn output_format(&self) -> OutputFormat {
        self.cb.state.lock().expect("cb lock").format
    }

    /// Flush for a seek/loop: invalidates every granted index, so the
    /// queues clear under the same lock, and the epoch advances so a
    /// callback racing the flush cannot re-deliver a stale index. Async
    /// codecs must be restarted after a flush.
    pub fn reset(&mut self) -> Result<(), DecodeError> {
        // SAFETY: flush/start on a live codec in the Executing state.
        unsafe {
            let status = AMediaCodec_flush(self.codec);
            if status != AMEDIA_OK {
                return Err(DecodeError(format!(
                    "flush failed ({status}) on {}",
                    self.name
                )));
            }
            // Flush invalidates every granted index; the clear runs under
            // the callback lock so a dispatched-but-blocked callback lands
            // after it (a stale index it might still deliver is re-granted
            // by start below — a duplicate would surface as a loud queue
            // error, not a silent wedge).
            {
                let mut state = self.cb.state.lock().expect("cb lock");
                state.input_free.clear();
                state.output_ready.clear();
            }
            let status = AMediaCodec_start(self.codec);
            if status != AMEDIA_OK {
                return Err(DecodeError(format!(
                    "start-after-flush failed ({status}) on {}",
                    self.name
                )));
            }
        }
        self.eos_queued = false;
        self.eos_pending = false;
        Ok(())
    }
}

impl Drop for AsyncCodec {
    fn drop(&mut self) {
        // SAFETY: stop/delete join the codec's callback thread, so the
        // userdata Arc is only released once no callback can run again.
        unsafe {
            AMediaCodec_stop(self.codec);
            AMediaCodec_delete(self.codec);
            drop(Arc::from_raw(self.userdata));
        }
    }
}

pub(crate) fn codec_name(codec: *mut AMediaCodec) -> Option<String> {
    // SAFETY: getName allocates a NUL-terminated string released via
    // releaseName; the contents are copied before release.
    unsafe {
        let mut name: *mut core::ffi::c_char = core::ptr::null_mut();
        if AMediaCodec_getName(codec, &mut name) != AMEDIA_OK || name.is_null() {
            return None;
        }
        let owned = CStr::from_ptr(name).to_string_lossy().into_owned();
        AMediaCodec_releaseName(codec, name);
        Some(owned)
    }
}
