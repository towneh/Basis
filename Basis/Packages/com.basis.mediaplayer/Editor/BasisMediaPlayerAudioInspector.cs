using System.IO;
using UnityEditor;
using UnityEditor.UIElements;
using UnityEngine.UIElements;

/// <summary>
/// Inspector for <see cref="BasisMediaPlayerAudio"/>: the layout lives in
/// UXML and the look in the shared stylesheet, so this is binding glue plus
/// the filter-ordering notice.
/// </summary>
[CustomEditor(typeof(BasisMediaPlayerAudio))]
public class BasisMediaPlayerAudioInspector : Editor
{
    private VisualElement _root;

    /// <summary>
    /// The package's own asset root, asked of the package manager rather than
    /// spelled out, so the paths survive the folder being renamed.
    /// </summary>
    private static string PackagePath => UnityEditor.PackageManager.PackageInfo
        .FindForAssembly(typeof(BasisMediaPlayerAudioInspector).Assembly)?.assetPath;

    public override VisualElement CreateInspectorGUI()
    {
        _root = new VisualElement();

        string package = PackagePath;
        if (string.IsNullOrEmpty(package))
        {
            _root.Add(new HelpBox("Could not resolve the package path for the media player editor assembly.", HelpBoxMessageType.Error));
            return _root;
        }

        var tree = AssetDatabase.LoadAssetAtPath<VisualTreeAsset>(
            Path.Combine(package, "Editor/StyleSheets/MediaPlayerAudioSDK.uxml").Replace('\\', '/'));
        var sheet = AssetDatabase.LoadAssetAtPath<StyleSheet>(
            Path.Combine(package, "Editor/StyleSheets/MediaPlayerSDK.uss").Replace('\\', '/'));
        if (tree == null)
        {
            _root.Add(new HelpBox("MediaPlayerAudioSDK.uxml missing.", HelpBoxMessageType.Error));
            return _root;
        }
        tree.CloneTree(_root);
        if (sheet != null) _root.styleSheets.Add(sheet);

        BindByName("OutputsField", "Outputs");
        BindByName("SampleRateField", "SampleRate");
        BindByName("ChannelCountField", "ChannelCount");
        BindByName("ClipLengthField", "ClipLengthSeconds");
        BindByName("AutoPlayField", "AutoPlayOnEnable");
        BindByName("StopOnDisableField", "StopOnDisable");
        BindByName("VolumeGainField", "VolumeGain");
        BindByName("MuteField", "Mute");
        _root.Bind(serializedObject);

        _root.Insert(0, new BasisMediaPlayerTapOrdering.Notice(
            () => (target as BasisMediaPlayerAudio)?.Outputs,
            names => $"Audio filters on {string.Join(", ", names)} won't be heard: the tap that " +
                     "generates the stream has to sit above them in the component list."));

        return _root;
    }

    private void BindByName(string name, string property)
    {
        if (_root.Q<VisualElement>(name) is IBindable bindable) bindable.bindingPath = property;
    }
}
