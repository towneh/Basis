using Basis.Scripts.BasisSdk.Players;
using Basis.Scripts.Drivers;
using UnityEngine;

[RequireComponent(typeof(MeshRenderer))]
public class BasisAdaptiveCircle : MonoBehaviour
{
    public MeshRenderer AdaptiveRenderer;

    static readonly int _ColorId = Shader.PropertyToID("_Color");
    static readonly int _RadiusId = Shader.PropertyToID("_Radius");      // world units
    static readonly int _ThickId = Shader.PropertyToID("_Thickness");   // world units

    const float PLANE_SIZE = 10f;    // Unity default plane
    const float MIN_SCALE = 0.1f;
    private const int RenderPriority = 2;
    MaterialPropertyBlock _mpb;

    public void Start()
    {
        BasisLocalPlayer.AfterSimulateOnRender.AddAction(RenderPriority, OnRender);
    }
    public void OnDestroy()
    {
        BasisLocalPlayer.AfterSimulateOnRender.RemoveAction(RenderPriority, OnRender);
    }
    private void OnRender()
    {
        var Position = BasisLocalCameraDriver.Position;

        float Yheight = BasisLocalPlayer.Instance.transform.position.y;
        this.transform.position = new Vector3(Position.x, Yheight + 0.02f, Position.z);
    }
    public void Apply(float radius, Color color)
    {
        _mpb ??= new MaterialPropertyBlock();

        // Make the plane at least as wide as the diameter
        float scale = Mathf.Max(MIN_SCALE, (radius * 2 / PLANE_SIZE) + 0.05f);
        transform.localScale = Vector3.one * scale;

        _mpb.Clear();
        _mpb.SetColor(_ColorId, color);
        _mpb.SetFloat(_RadiusId, Mathf.Max(0f, radius / PLANE_SIZE));        // pass world radius directly
        _mpb.SetFloat(_ThickId, 0.05f);      // pass world thickness directly
        AdaptiveRenderer.SetPropertyBlock(_mpb);
    }
}
