using System;
using Basis.BasisUI;

/// <summary>
/// Per-user consent gate for room-shared / user-entered media URLs. This is the
/// "has the user approved this origin?" layer, separate from the host-safety floor
/// (<see cref="BasisMediaPlayerSecurity.IsUrlAllowed"/>, which still runs at load).
/// Consent attaches to the originating URL only; whatever it resolves to (a CDN URL,
/// or a split video+audio pair) is a derivation that rides the safety floor and is
/// never re-prompted. Mirrors the consent flow in com.basis.shim/VideoPlayerShim,
/// reusing the framework's <see cref="BasisTrustedUrls"/> + URL prompt panel. Admins
/// (the control/* permission) bypass the prompt — they are the trusted operators who set URLs.
/// </summary>
public static class BasisMediaPlayerConsent
{
    /// <summary>
    /// Runs <paramref name="onAllowed"/> immediately when <paramref name="url"/> needs no
    /// consent (transport scheme or local file) or is already trusted; otherwise pops the
    /// framework URL prompt and runs <paramref name="onAllowed"/> only on accept, persisting
    /// the choice at the chosen scope. The prompt is async (a frame or more), so callers must
    /// guard against being superseded in the continuation.
    /// </summary>
    public static void Gate(string url, Action onAllowed)
    {
        if (onAllowed == null) return;

        // Admins (the basis.mediaplayer.control or * permission) bypass the prompt: they are the
        // trusted operators permitted to set URLs, so prompting them is pointless — and it gives
        // headless / automated clients a way to load without a dialog they can't show, by granting
        // the test client the control permission. Offline (no network permissions) IsLocalAdmin is
        // false, so consent still applies in single-player.
        if (!NeedsConsent(url)
            || BasisMediaPlayerNetworking.IsLocalAdmin()
            || BasisTrustedUrls.IsTrusted(url))
        {
            onAllowed();
            return;
        }

        // A malformed http(s) URL still gates; it falls to the safety floor at load.
        Uri uri = null;
        try { uri = new Uri(url); } catch { /* leave uri null; remember-by-scope no-ops below */ }

        BasisMenuURLPromptPanel.CreateNew(
            url,
            response =>
            {
                if (!response.Accepted) return;
                if (response.RememberChoice) Remember(url, uri, response.Scope);
                onAllowed();
            },
            divertible: true);
    }

    /// <summary>
    /// Consent applies to http/https origins only. Explicit transport schemes
    /// (rtsp/rtspt/rtmp/rtmps/rist) and local files ride the host-safety floor without a
    /// prompt — they are typed addresses, not "a random link", and <see cref="BasisTrustedUrls"/>
    /// can't persist non-https anyway.
    /// </summary>
    public static bool NeedsConsent(string url)
        => !string.IsNullOrEmpty(url)
        && (url.StartsWith("http://", StringComparison.OrdinalIgnoreCase)
         || url.StartsWith("https://", StringComparison.OrdinalIgnoreCase));

    private static void Remember(string url, Uri uri, BasisMenuURLPromptPanel.RememberChoiceScope scope)
    {
        switch (scope)
        {
            case BasisMenuURLPromptPanel.RememberChoiceScope.URL:
                BasisTrustedUrls.Add(url);
                break;
            case BasisMenuURLPromptPanel.RememberChoiceScope.Hostname:
                if (uri != null) BasisTrustedUrls.Add(uri.Scheme + "://" + uri.Host + "/*");
                break;
            case BasisMenuURLPromptPanel.RememberChoiceScope.Domain:
                if (uri != null) BasisTrustedUrls.Add(uri.Scheme + "://*." + RegistrableDomain(uri) + "/*");
                break;
        }
    }

    private static string RegistrableDomain(Uri uri)
    {
        string[] parts = uri.Host.Split('.');
        return parts.Length >= 2 ? parts[parts.Length - 2] + "." + parts[parts.Length - 1] : uri.Host;
    }
}
