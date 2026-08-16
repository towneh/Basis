/// <summary>
/// One audio track the current source offers. Multi-language films carry
/// one per dub; a screen recording carries one per capture device, which
/// is why neither Language nor Label can be relied on to be present.
/// </summary>
public sealed class BasisAudioTrack
{
    /// <summary>Position in <see cref="BasisMediaPlayer.AudioTracks"/>, which
    /// is what selection is keyed on.</summary>
    public int Index = -1;

    /// <summary>The container's own track number. Diagnostic only.</summary>
    public int TrackId;

    /// <summary>ISO 639 code, or null when the container states none.</summary>
    public string Language;

    /// <summary>Track name where the container carries one. Matroska does;
    /// MP4's media header does not, so recordings muxed to MP4 arrive
    /// unnamed.</summary>
    public string Label;

    public int ChannelCount;

    public int SampleRate;

    /// <summary>What a picker should show. Prefers the track's own name,
    /// then its language, and always carries the position — three unnamed
    /// stereo tracks from a screen recording would otherwise render as
    /// three identical rows.</summary>
    public string DisplayName
    {
        get
        {
            string channels = ChannelCount > 0 ? $"{ChannelCount}ch" : null;
            string name = !string.IsNullOrEmpty(Label) ? Label : Language;
            string ordinal = $"Track {Index + 1}";
            if (string.IsNullOrEmpty(name))
                return channels != null ? $"{ordinal} · {channels}" : ordinal;
            return channels != null ? $"{name} · {channels}" : name;
        }
    }
}
