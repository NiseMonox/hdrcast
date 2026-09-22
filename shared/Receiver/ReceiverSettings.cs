using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.Serialization;
using HdrCast.Common;

namespace HdrCast.Receiver
{
    public enum DisplayMode
    {
        /// <summary>--target-colorspace-hint=auto (passthrough only when Windows HDR is on).</summary>
        Auto = 0,
        /// <summary>--target-colorspace-hint=yes (PQ swapchain; Windows HDR must be on).</summary>
        HdrPassthrough = 1,
        /// <summary>--target-colorspace-hint=no plus --tone-mapping=&lt;algorithm&gt;.</summary>
        SdrToneMap = 2,
    }

    public enum HwDecMode
    {
        AutoSafe = 0,
        D3d11va = 1,
        D3d11vaCopy = 2,
        No = 3,
    }

    public enum ReceiverFrontend
    {
        /// <summary>mpv opens srt:// itself (needs a libsrt-enabled mpv build: shinchiro / zhongfly).</summary>
        MpvDirect = 0,
        /// <summary>ffmpeg -i srt://127.0.0.1:… -c copy -f mpegts pipe:1 | mpv - (fallback when mpv lacks libsrt).</summary>
        FfmpegPipe = 1,
    }

    public sealed class LatencyPreset
    {
        public string Name { get; }
        public int LatencyMs { get; }
        public LatencyPreset(string name, int latencyMs) { Name = name; LatencyMs = latencyMs; }
        public override string ToString() => Name + " (" + LatencyMs + " ms)";
    }

    /// <summary>Persisted to %APPDATA%\HdrCast\receiver.json via DataContractJsonSerializer.</summary>
    [DataContract]
    public sealed record ReceiverSettings
    {
        public const string FileName = "receiver.json";
        public const string DefaultToneMapping = "bt.2390";

        public static readonly LatencyPreset[] LatencyPresets =
        {
            new LatencyPreset("Japan", 1500),
            new LatencyPreset("Australia", 2000),
            new LatencyPreset("China", 3500),
        };

        public static readonly string[] ToneMappingAlgorithms =
        {
            "bt.2390", "auto", "spline", "bt.2446a", "st2094-40", "st2094-10", "hable", "mobius", "reinhard", "clip",
        };

        public ReceiverSettings() { SetDefaults(); }

        // The old relay/direct fields (mode, host, port, path, user, password, passphrase) are ignored in existing files.
        /// <summary>SRT latency this viewer asks the host for (its region preset).</summary>
        [DataMember(Name = "latencyMs", Order = 7)] public int LatencyMs { get; set; }
        [DataMember(Name = "expectedBitrateMbps", Order = 8)] public int ExpectedBitrateMbps { get; set; }
        [DataMember(Name = "displayMode", Order = 9)] public DisplayMode DisplayMode { get; set; }
        [DataMember(Name = "toneMapping", Order = 10)] public string ToneMapping { get; set; } = "";
        [DataMember(Name = "hwDec", Order = 11)] public HwDecMode HwDec { get; set; }
        [DataMember(Name = "bufferSeconds", Order = 12)] public double BufferSeconds { get; set; }
        [DataMember(Name = "fullscreen", Order = 13)] public bool Fullscreen { get; set; }
        [DataMember(Name = "fullscreenScreen", Order = 14)] public int FullscreenScreen { get; set; }
        [DataMember(Name = "frontend", Order = 15)] public ReceiverFrontend Frontend { get; set; }
        [DataMember(Name = "mpvPath", Order = 16)] public string MpvPath { get; set; } = "";
        [DataMember(Name = "ffmpegPath", Order = 17)] public string FfmpegPath { get; set; } = "";
        [DataMember(Name = "autoCatchUp", Order = 18)] public bool AutoCatchUp { get; set; }
        /// <summary>0 = let mpv/Windows decide; otherwise --target-peak=&lt;nits&gt; (bogus display peaks).</summary>
        [DataMember(Name = "targetPeakNits", Order = 19)] public int TargetPeakNits { get; set; }
        /// <summary>The host (peer id) watched last, preselected in the live list.</summary>
        [DataMember(Name = "lastHost", Order = 21)] public string LastHost { get; set; } = "";

        public void SetDefaults()
        {
            LatencyMs = 1500;
            ExpectedBitrateMbps = 100;
            DisplayMode = DisplayMode.Auto;
            ToneMapping = DefaultToneMapping;
            HwDec = HwDecMode.D3d11va;
            BufferSeconds = 1.5;
            Fullscreen = false;
            FullscreenScreen = 0;
            Frontend = ReceiverFrontend.MpvDirect;
            MpvPath = "";
            FfmpegPath = "";
            AutoCatchUp = true;
            TargetPeakNits = 0;
            LastHost = "";
        }

        [OnDeserializing]
        private void OnDeserializing(StreamingContext context) => SetDefaults();

        /// <summary>Repairs nulls / out-of-range values after loading a hand-edited file.</summary>
        public ReceiverSettings Normalize()
        {
            LastHost = (LastHost ?? "").Trim();
            ToneMapping = string.IsNullOrWhiteSpace(ToneMapping) ? DefaultToneMapping : ToneMapping.Trim();
            MpvPath ??= "";
            FfmpegPath ??= "";
            if (LatencyMs < 20) LatencyMs = 1500;
            if (LatencyMs > 30000) LatencyMs = 30000;
            if (ExpectedBitrateMbps < 1) ExpectedBitrateMbps = 100;
            if (ExpectedBitrateMbps > 1000) ExpectedBitrateMbps = 1000;
            if (double.IsNaN(BufferSeconds) || BufferSeconds < 0.1) BufferSeconds = 1.5;
            if (BufferSeconds > 30) BufferSeconds = 30;
            if (FullscreenScreen < 0) FullscreenScreen = 0;
            // mpv accepts "auto" or 10..10000; anything in between makes it exit with
            // "Invalid value for option target-peak", so treat a too-small number as auto.
            if (TargetPeakNits < 10) TargetPeakNits = 0;
            if (TargetPeakNits > 10000) TargetPeakNits = 10000;
            if (!Enum.IsDefined(typeof(DisplayMode), DisplayMode)) DisplayMode = DisplayMode.Auto;
            if (!Enum.IsDefined(typeof(HwDecMode), HwDec)) HwDec = HwDecMode.D3d11va;
            if (!Enum.IsDefined(typeof(ReceiverFrontend), Frontend)) Frontend = ReceiverFrontend.MpvDirect;
            return this;
        }

        /// <summary>Problems that prevent playing (empty when valid).</summary>
        public List<string> Validate()
        {
            var problems = new List<string>();
            if (LatencyMs < 20) problems.Add("Latency must be at least 20 ms.");
            if (ExpectedBitrateMbps < 1) problems.Add("Expected bitrate must be at least 1 Mbps.");
            if (BufferSeconds <= 0) problems.Add("Buffer seconds must be positive.");
            return problems;
        }

        /// <summary>oheadbw this viewer asks the host for: the cross-border (China) preset gets more retransmission headroom.</summary>
        public int OverheadPercent => LatencyMs >= 3000 ? 50 : 25;

        /// <summary>
        /// mpv's SRT URL: a caller to the local end of the punched path (<see cref="HdrCast.P2p.ViewerLink.LocalPort"/>)
        /// with the group's passphrase; the receive buffer is sized for this viewer's latency and expected bitrate.
        /// </summary>
        public SrtUrlOptions ToSrtOptions(int localPort, string passphrase)
        {
            return new SrtUrlOptions
            {
                Topology = SrtTopology.Caller,
                Role = SrtRole.Read,
                Host = "127.0.0.1",
                Port = localPort,
                Passphrase = string.IsNullOrEmpty(passphrase) ? null : passphrase,
                PbKeyLen = 32,
                LatencyMs = LatencyMs,
                BitrateBps = (long)ExpectedBitrateMbps * 1_000_000L,
                RateControl = false,
                ConnectTimeoutMs = 5000,
                IncludeRecvSocketBuffer = true,
            };
        }

        public static string DefaultPath => AppPaths.SettingsFile(FileName);

        public static ReceiverSettings Load(string path)
        {
            var s = Json.LoadFile<ReceiverSettings>(path);
            return (s ?? new ReceiverSettings()).Normalize();
        }

        public void Save(string path) => Json.SaveFile(path, this);
    }
}
