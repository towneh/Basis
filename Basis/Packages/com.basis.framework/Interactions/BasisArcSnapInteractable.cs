using Basis.Scripts.Device_Management.Devices;
using Basis.Scripts.TransformBinders.BoneControl;
using UnityEngine;

namespace Basis.Scripts.BasisSdk.Interactions
{
    /// <summary>
    /// Snap-path interactable whose snap points lie on an arc. Position is the snap point's
    /// world position plus a fixed world offset captured at Awake, so the scrubber traces the
    /// marker arc regardless of how individual markers are oriented. Rotation tracks
    /// <see cref="pivot"/> (when set) plus an angular delta around the arc's rotation axis
    /// equal to the angle swept from the resting marker to the current marker, so the scrubber
    /// rotates with the arc as it moves between snap points.
    ///
    /// The arc rotation axis is derived from the marker world positions (cross product of two
    /// chords), not from the pivot's authored rotation. The pivot transform is consulted for
    /// position-as-centre and rotation-tracking, but its own up-axis is not assumed to align
    /// with the arc plane's normal.
    ///
    /// Index selection runs entirely in world space:
    /// * A hand whose bone is within <see cref="BasisInteractableObject.GrabRadius"/>×2 of the
    ///   scrubber selects the marker closest to the bone position.
    /// * Otherwise (hand laser, desktop center-eye), the marker with the smallest perpendicular
    ///   distance to the input's <see cref="BasisInput.RaycastCoord"/> ray wins.
    /// </summary>
    public class BasisArcSnapInteractable : BasisSnapPathInteractable
    {
        [Header("Arc")]
        [Tooltip("Rotation source for the scrubber. The scrubber's rotation is captured relative to the pivot at Awake; on each EvaluateAtIndex it's re-applied with an additional rotation around the marker-derived arc axis equal to the arc-angle swept from the resting marker, so the scrubber rotates with the arc as it moves. Leave empty to keep the authored world rotation and skip arc-angle rotation.")]
        public Transform pivot;

        private Vector3 _restingPositionOffset;
        private Quaternion _restingLocalRotation;
        private Vector3 _arcAxisWorld;
        private int _restingIndex;
        private bool _arcReady;

        public override void Awake()
        {
            base.Awake();
            if (snapPoints == null || snapPoints.Length == 0) return;
            if (CurrentIndex < 0 || CurrentIndex >= snapPoints.Length) return;
            Transform initial = snapPoints[CurrentIndex];
            if (initial == null) return;

            _restingPositionOffset = transform.position - initial.position;
            _restingLocalRotation = pivot != null
                ? Quaternion.Inverse(pivot.rotation) * transform.rotation
                : transform.rotation;
            _restingIndex = CurrentIndex;
            _arcAxisWorld = ComputeArcAxis();
            _arcReady = true;

            EvaluateAtIndex(CurrentIndex, out Vector3 p, out Quaternion r);
            transform.SetPositionAndRotation(p, r);
        }

        protected override void EvaluateAtIndex(int index, out Vector3 position, out Quaternion rotation)
        {
            if (!_arcReady)
            {
                base.EvaluateAtIndex(index, out position, out rotation);
                return;
            }

            if (pivot == null || _arcAxisWorld.sqrMagnitude < 1e-8f)
            {
                position = snapPoints[index].position + _restingPositionOffset;
                rotation = pivot != null ? pivot.rotation * _restingLocalRotation : _restingLocalRotation;
                return;
            }

            float angularDelta = ComputeArcAngle(_restingIndex, index);
            Quaternion arcRotation = Quaternion.AngleAxis(angularDelta, _arcAxisWorld);
            position = snapPoints[index].position + arcRotation * _restingPositionOffset;
            rotation = arcRotation * pivot.rotation * _restingLocalRotation;
        }

        /// <summary>
        /// Plane normal of the marker arc, derived from the cross product of the chord
        /// <c>snapPoints[mid] - snapPoints[0]</c> with <c>snapPoints[last] - snapPoints[mid]</c>.
        /// Sign is flipped if necessary so that traversing 0 → last is a positive angle around
        /// the returned axis (right-hand rule). Returns <see cref="Vector3.zero"/> when the
        /// markers are colinear or there are fewer than three.
        /// </summary>
        private Vector3 ComputeArcAxis()
        {
            if (snapPoints == null || snapPoints.Length < 3) return Vector3.zero;
            int last = snapPoints.Length - 1;
            int mid = snapPoints.Length / 2;
            if (snapPoints[0] == null || snapPoints[mid] == null || snapPoints[last] == null) return Vector3.zero;

            Vector3 v1 = snapPoints[mid].position - snapPoints[0].position;
            Vector3 v2 = snapPoints[last].position - snapPoints[mid].position;
            Vector3 axis = Vector3.Cross(v1, v2);
            if (axis.sqrMagnitude < 1e-8f) return Vector3.zero;
            axis.Normalize();

            if (pivot != null)
            {
                Vector3 pivotPos = pivot.position;
                Vector3 dir0 = Vector3.ProjectOnPlane(snapPoints[0].position - pivotPos, axis);
                Vector3 dirLast = Vector3.ProjectOnPlane(snapPoints[last].position - pivotPos, axis);
                if (dir0.sqrMagnitude > 1e-8f && dirLast.sqrMagnitude > 1e-8f
                    && Vector3.SignedAngle(dir0, dirLast, axis) < 0f)
                {
                    axis = -axis;
                }
            }
            return axis;
        }

        /// <summary>
        /// Signed angle (in degrees) around the cached arc axis from <paramref name="fromIndex"/>'s
        /// projected direction to <paramref name="toIndex"/>'s, measured from the current pivot
        /// position so the rotation stays correct if the pivot itself moves.
        /// </summary>
        private float ComputeArcAngle(int fromIndex, int toIndex)
        {
            if (snapPoints[fromIndex] == null || snapPoints[toIndex] == null) return 0f;
            Vector3 pivotPos = pivot.position;
            Vector3 dirFrom = Vector3.ProjectOnPlane(snapPoints[fromIndex].position - pivotPos, _arcAxisWorld);
            Vector3 dirTo = Vector3.ProjectOnPlane(snapPoints[toIndex].position - pivotPos, _arcAxisWorld);
            if (dirFrom.sqrMagnitude < 1e-8f || dirTo.sqrMagnitude < 1e-8f) return 0f;
            return Vector3.SignedAngle(dirFrom, dirTo, _arcAxisWorld);
        }

        protected override int FindNearestIndex(BasisInputWrapper interacting)
        {
            if (!_arcReady) return CurrentIndex;
            if (snapPoints == null || snapPoints.Length == 0) return CurrentIndex;

            Vector3 handPos = interacting.BoneControl.OutgoingWorldData.position;
            bool isHandRole = interacting.Role == BasisBoneTrackedRole.LeftHand
                              || interacting.Role == BasisBoneTrackedRole.RightHand;
            float grabReach = GrabRadius * 2f;
            bool directGrab = isHandRole && (handPos - transform.position).sqrMagnitude <= grabReach * grabReach;

            BasisInput source = interacting.Source;
            bool useRay = !directGrab && source != null;
            Vector3 rayOrigin = useRay ? source.RaycastCoord.position : default;
            Vector3 rayDir = useRay
                ? (source.RaycastCoord.rotation * Vector3.forward).normalized
                : default;

            int nearest = CurrentIndex;
            float bestScore = float.MaxValue;
            for (int i = 0; i < snapPoints.Length; i++)
            {
                Transform marker = snapPoints[i];
                if (marker == null) continue;
                Vector3 markerPos = marker.position;
                float score;
                if (useRay)
                {
                    Vector3 toMarker = markerPos - rayOrigin;
                    float t = Vector3.Dot(toMarker, rayDir);
                    if (t < 0f) continue;
                    Vector3 closestOnRay = rayOrigin + rayDir * t;
                    score = (markerPos - closestOnRay).sqrMagnitude;
                }
                else
                {
                    score = (markerPos - handPos).sqrMagnitude;
                }
                if (score < bestScore)
                {
                    bestScore = score;
                    nearest = i;
                }
            }
            return nearest;
        }
    }
}
