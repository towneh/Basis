using System.IO;
using UnityEditor;
using UnityEngine;

namespace Basis.Media
{
    /// <summary>
    /// Inspector for <see cref="BasisMediaPlayer"/>: live session readout and
    /// transport controls while playing, plus a scene-setup menu item so a
    /// test player is one click away in any project referencing the package.
    /// </summary>
    [CustomEditor(typeof(BasisMediaPlayer))]
    public class BasisMediaPlayerEditor : Editor
    {
        public override bool RequiresConstantRepaint() => Application.isPlaying;

        public override void OnInspectorGUI()
        {
            DrawDefaultInspector();

            var player = (BasisMediaPlayer)target;
            if (!Application.isPlaying)
                return;

            EditorGUILayout.Space();
            EditorGUILayout.LabelField("Session", EditorStyles.boldLabel);
            EditorGUILayout.LabelField("State", player.State.ToString());
            EditorGUILayout.LabelField("Position",
                $"{player.PositionSeconds:F2}s / {player.DurationSeconds:F2}s");
            EditorGUILayout.LabelField("Banked", $"{player.BankedMilliseconds} ms");
            EditorGUILayout.LabelField("Frames",
                $"decoded {player.FramesDecoded}, presented {player.FramesPresented}");
            EditorGUILayout.LabelField("Audio", $"{player.AudioFramesPulled} frames pulled");
            if (player.State == BmState.Error)
                EditorGUILayout.HelpBox($"Error code {player.ErrorCode}", MessageType.Error);

            EditorGUILayout.BeginHorizontal();
            if (GUILayout.Button("Open"))
                player.Open(player.url);
            if (GUILayout.Button("Play"))
                player.Play();
            if (GUILayout.Button("Pause"))
                player.Pause();
            if (GUILayout.Button("-10s"))
                player.Seek(player.PositionSeconds - 10.0);
            if (GUILayout.Button("+10s"))
                player.Seek(player.PositionSeconds + 10.0);
            if (GUILayout.Button("Close"))
                player.Close();
            EditorGUILayout.EndHorizontal();
        }

        [MenuItem("Basis/Tools/Media Player/Create Test Player")]
        static void CreateTestPlayer() => CreateTestPlayer(surround: false);

        [MenuItem("Basis/Tools/Media Player/Create Test Player (Surround)")]
        static void CreateSurroundTestPlayer() => CreateTestPlayer(surround: true);

        /// <summary>
        /// A screen, a player and its audio outputs: one unspatialised stereo
        /// output, or the eight positioned speakers a 5.1 / 7.1 mix wants. The
        /// arrangement matches the shipped prefabs, so what this drops in the
        /// scene is what a world author would have built by hand.
        /// </summary>
        static void CreateTestPlayer(bool surround)
        {
            const float screenWidth = 16f / 9f * 2f;

            var quad = GameObject.CreatePrimitive(PrimitiveType.Quad);
            quad.name = "BasisMedia Screen";
            quad.transform.localScale = new Vector3(screenWidth, 2f, 1f);
            Object.DestroyImmediate(quad.GetComponent<Collider>());

            // Resolved by shader name, so the screen picks up the Basis video
            // shader wherever it ships from — it paints out-of-range UVs black,
            // which is what letterboxing an off-aspect source needs.
            var shader = Shader.Find("Basis/Media Player Video")
                         ?? Shader.Find("Universal Render Pipeline/Unlit")
                         ?? Shader.Find("Unlit/Texture");
            if (shader != null)
                quad.GetComponent<MeshRenderer>().sharedMaterial = new Material(shader);

            var go = new GameObject("BasisMediaPlayer");
            go.transform.position = quad.transform.position;
            var player = go.AddComponent<BasisMediaPlayer>();
            player.targetRenderer = quad.GetComponent<MeshRenderer>();
            player.url = DefaultFixtureUrl();

            if (surround) BasisMediaAudioRig.AddSurroundOutputs(go, screenWidth);
            else BasisMediaAudioRig.AddStereoOutput(go);

            Selection.activeGameObject = go;
            UnityEditor.SceneManagement.EditorSceneManager.MarkSceneDirty(go.scene);
            Debug.Log("[BasisMedia] test player created; set a URL and enter play mode.");
        }

        /// <summary>The repo's A/V fixture when the package is referenced
        /// straight from a basis-media checkout; empty otherwise.</summary>
        static string DefaultFixtureUrl()
        {
            // Asked of the package manager rather than spelled out, so a folder
            // rename does not quietly turn this into an empty string.
            string packagePath = UnityEditor.PackageManager.PackageInfo
                .FindForAssembly(typeof(BasisMediaPlayerEditor).Assembly)?.assetPath;
            if (string.IsNullOrEmpty(packagePath)) return string.Empty;
            packagePath = Path.GetFullPath(packagePath);
            string fixture = Path.GetFullPath(
                Path.Combine(packagePath, "Native~", "fixtures", "h264-aac-640x360-30fps.mp4"));
            return File.Exists(fixture) ? fixture : string.Empty;
        }
    }
}
