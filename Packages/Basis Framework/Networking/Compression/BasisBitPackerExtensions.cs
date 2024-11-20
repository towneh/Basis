﻿using Basis.Scripts.Networking.NetworkedAvatar;
using DarkRift;
using Unity.Burst;
using UnityEngine;

namespace Basis.Scripts.Networking.Compression
{
    public static class BasisBitPackerExtensions
    {
        public static void WriteUshortFloat(DarkRiftWriter bitPacker, float value, BasisRangedUshortFloatData compressor)
        {
            bitPacker.Write(compressor.Compress(value));
        }
        public static float ReadUshortFloat(this DarkRiftReader bitPacker, BasisRangedUshortFloatData compressor)
        {
            bitPacker.Read(out ushort data);
            return compressor.Decompress(data);
        }
        public static void ReadUshortFloat(this DarkRiftReader bitPacker, BasisRangedUshortFloatData compressor,ref float Value)
        {
            bitPacker.Read(out ushort data);
            Value = compressor.Decompress(data);
        }
        public static void WriteUshortVectorFloat(DarkRiftWriter bitPacker, Vector3 values, BasisRangedUshortFloatData compressor)
        {
            ushort Compressx = compressor.Compress(values.x);
            ushort Compressy = compressor.Compress(values.y);
            ushort Compressz = compressor.Compress(values.z);
            bitPacker.Write(Compressx);
            bitPacker.Write(Compressy);
            bitPacker.Write(Compressz);
        }
        public static void ReadVectorFloat(this DarkRiftReader bitPacker, out float X, out float Y, out float Z)
        {
            bitPacker.Read(out X);
            bitPacker.Read(out Y);
            bitPacker.Read(out Z);
        }
        public static void WriteVectorFloat(DarkRiftWriter bitPacker, Vector3 values)
        {
            bitPacker.Write(values.x);
            bitPacker.Write(values.y);
            bitPacker.Write(values.z);
        }
    }
}