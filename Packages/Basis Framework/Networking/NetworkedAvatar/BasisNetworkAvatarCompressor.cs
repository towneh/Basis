using Basis.Scripts.Networking.Compression;
using Basis.Scripts.Profiler;
using DarkRift;
using DarkRift.Server.Plugins.Commands;
using System;
using UnityEngine;
using static Basis.Scripts.Networking.NetworkedAvatar.BasisNetworkSendBase;
using static SerializableDarkRift;

namespace Basis.Scripts.Networking.NetworkedAvatar
{
    public static class BasisNetworkAvatarCompressor
    {
        public static void Compress(BasisNetworkSendBase NetworkSendBase, Animator Anim)
        {
            using (DarkRiftWriter writer = DarkRiftWriter.Create())
            {
                CompressAvatarData(NetworkSendBase, Anim);

                NetworkSendBase.ReSizeAndErrorIfNeeded();

                writer.Write(NetworkSendBase.LASM);
                BasisNetworkProfiler.AvatarUpdatePacket.Sample(writer.Length);

                using (var msg = Message.Create(BasisTags.AvatarMuscleUpdateTag, writer))
                {
                    BasisNetworkManagement.Instance.Client.SendMessage(msg, BasisNetworking.MovementChannel, DeliveryMethod.Sequenced);
                }
            }
        }

        public static void CompressAvatarData(BasisNetworkSendBase NetworkSendBase, Animator Anim)
        {
            // Get the pose from the PoseHandler
            HumanPose CachedPose = new HumanPose();
            NetworkSendBase.PoseHandler.GetHumanPose(ref CachedPose);

            // Fetch the local player's avatar data and update its components
            NetworkSendBase.LastAvatarBuffer.Position = Anim.bodyPosition;
            NetworkSendBase.LastAvatarBuffer.Scale = Anim.transform.localScale;
            NetworkSendBase.LastAvatarBuffer.rotation = Anim.bodyRotation;

            // Ensure muscles are properly populated or copied
            if (NetworkSendBase.LastAvatarBuffer.Muscles == null || NetworkSendBase.LastAvatarBuffer.Muscles.Length != BasisCompressionOfMuscles.BoneLength)
            {
                NetworkSendBase.LastAvatarBuffer.Muscles = CachedPose.muscles;
            }
            else
            {
                Buffer.BlockCopy(CachedPose.muscles, 0, NetworkSendBase.LastAvatarBuffer.Muscles, 0, BasisCompressionOfMuscles.BoneLength);
            }

            // Now that all components are updated, we can compress the avatar update
            CompressAvatarUpdate(ref NetworkSendBase.LASM, NetworkSendBase.LastAvatarBuffer, NetworkSendBase.PositionRanged, NetworkSendBase.ScaleRanged);
        }
        public static void CompressAvatarUpdate(ref LocalAvatarSyncMessage syncmessage, AvatarBuffer AvatarBuffer, BasisRangedUshortFloatData PositionRanged, BasisRangedUshortFloatData ScaleRanged)
        {
            if (syncmessage.array != null || syncmessage.array.Length != TotalArraySize)
            {
                // Allocate the total buffer size at once
                syncmessage.array = new byte[TotalArraySize];
            }

            // Compress Position (HipsPosition) and Scale directly into the array
            int offset = 0;
            WriteVectorFloatToBuffer(AvatarBuffer.Position, ref syncmessage.array, ref offset);
            CompressUShortVector3ToBuffer(AvatarBuffer.Scale, ScaleRanged,ref syncmessage.array, ref offset);

            // Compress Rotation (Quaternion) into the array
            CompressQuaternionToBuffer(AvatarBuffer.rotation, ref syncmessage.array, ref offset);

            // Compress Muscles data into the array
            BasisCompressionOfMuscles.CompressMusclesToBuffer(AvatarBuffer.Muscles, ref syncmessage.array, ref offset);
        }

        public static void WriteVectorFloatToBuffer(Vector3 values,ref byte[] buffer, ref int offset)
        {
            BitConverter.GetBytes(values.x).CopyTo(buffer, offset);
            offset += 4;
            BitConverter.GetBytes(values.y).CopyTo(buffer, offset);
            offset += 4;
            BitConverter.GetBytes(values.z).CopyTo(buffer, offset);
            offset += 4;
        }

        public static void CompressUShortVector3ToBuffer(Vector3 input, BasisRangedUshortFloatData compressor,ref byte[] buffer, ref int offset)
        {
            ushort x = compressor.Compress(input.x);
            ushort y = compressor.Compress(input.y);
            ushort z = compressor.Compress(input.z);

            Buffer.BlockCopy(BitConverter.GetBytes(x), 0, buffer, offset, 2);
            offset += 2;
            Buffer.BlockCopy(BitConverter.GetBytes(y), 0, buffer, offset, 2);
            offset += 2;
            Buffer.BlockCopy(BitConverter.GetBytes(z), 0, buffer, offset, 2);
            offset += 2;
        }

        public static void CompressQuaternionToBuffer(Quaternion rotation,ref byte[] buffer, ref int offset)
        {
            Buffer.BlockCopy(BitConverter.GetBytes(rotation.x), 0, buffer, offset, 4);
            offset += 4;
            Buffer.BlockCopy(BitConverter.GetBytes(rotation.y), 0, buffer, offset, 4);
            offset += 4;
            Buffer.BlockCopy(BitConverter.GetBytes(rotation.z), 0, buffer, offset, 4);
            offset += 4;

            ushort compressedW = BasisNetworkSendBase.RotationCompressor.Compress(rotation.w);
            Buffer.BlockCopy(BitConverter.GetBytes(compressedW), 0, buffer, offset, 2);
            offset += 2;
        }
    }
}