using System;
using System.Collections.Generic;
using System.Globalization;
using System.Runtime.Serialization;
using HdrCast.Capture;
using HdrCast.Common;

namespace HdrCast.Sender.Model
{
    public enum SendMode
    {
        /// <summary>Publish to the MediaMTX relay (streamid=publish:path:user:pass).</summary>
        Relay = 0,
        /// <summary>Direct: this PC listens (mode=listener); the viewer dials in. Needs a UDP port forward here.</summary>
        DirectListen = 1,
        /// <summary>Direct: this PC dials the viewer (mode=caller). The viewer needs the port forward.</summary>
        DirectDial = 2,
    }

    /// <summary>How the output resolution is chosen.</summary>
    public enum ScaleMode
    {
        Native = 0,
        H1440 = 1,
        H1080 = 2,
        H720 = 3,
    }

    /// <summary>HDR handling. Auto follows the monitor; ForceSdr streams BT.709 even from an HDR desktop.</summary>
    public enum HdrMode
    {
        Auto = 0,
        ForceSdr = 1,
    }

    public sealed class RegionPreset
    {
        public string Name { get; }
        public int LatencyMs { get; }
        public int OverheadPercent { get; }
        public RegionPreset(string name, int latencyMs, int overheadPercent)
        {
            Name = name; LatencyMs = latencyMs; OverheadPercent = overheadPercent;
        }
        public override string ToString() => Name + " (" + LatencyMs + " ms)";
    }

    /// <summary>Persisted to %APPDATA%\HdrCast\sender.json via DataContractJsonSerializer.</summary>
    [DataContract]
    public sealed class SenderSettings
    {
        public const string FileName = "sender.json";

        /// <summary>Relay hop is on the LAN, so it only needs enough latency to absorb scheduling jitter.</summary>
        public const int RelayPublishLatencyMs = 800;

        public static readonly RegionPreset[] RegionPresets =
        {
            new RegionPreset("LAN / relay", 800, 25),
            new RegionPreset("Japan", 1500, 25),
            new RegionPreset("Australia", 2000, 25),
            new RegionPreset("China", 3500, 50),
        };

        public SenderSettings() { SetDefaults(); }

        // ---- capture ----
        [DataMember(Name = "outputIndex", Order = 0)] public int OutputIndex { get; set; }
        /// <summary>Remembered monitor identity so a changed DXGI order can be corrected on the next start.</summary>
        [DataMember(Name = "outputDeviceName", Order = 1)] public string OutputDeviceName { get; set; } = "";
        [DataMember(Name = "outputFriendlyName", Order = 2)] public string OutputFriendlyName { get; set; } = "";
        [DataMember(Name = "fps", Order = 3)] public int Fps { get; set; }
        [DataMember(Name = "scale", Order = 4)] public ScaleMode Scale { get; set; }
        [DataMember(Name = "hdrMode", Order = 5)] public HdrMode HdrMode { get; set; }
        [DataMember(Name = "cursor", Order = 6)] public bool Cursor { get; set; }
        [DataMember(Name = "sdrWhite", Order = 7)] public double SdrWhite { get; set; }

        // ---- encoding ----
        [DataMember(Name = "encoder", Order = 10)] public VideoEncoder Encoder { get; set; }
        [DataMember(Name = "quality", Order = 11)] public QualityPreset Quality { get; set; }
        [DataMember(Name = "bitrateMbps", Order = 12)] public int BitrateMbps { get; set; }
        [DataMember(Name = "gopSeconds", Order = 13)] public double GopSeconds { get; set; }
        [DataMember(Name = "bufferSeconds", Order = 14)] public double BufferSeconds { get; set; }
        [DataMember(Name = "bFrames", Order = 15)] public bool BFrames { get; set; }
        [DataMember(Name = "lookahead", Order = 16)] public bool Lookahead { get; set; }

        // ---- audio ----
        [DataMember(Name = "audio", Order = 20)] public bool Audio { get; set; }
        [DataMember(Name = "audioDevice", Order = 21)] public string AudioDevice { get; set; } = "default";
        [DataMember(Name = "audioCodec", Order = 22)] public AudioCodec AudioCodec { get; set; }
        [DataMember(Name = "audioBitrateKbps", Order = 23)] public int AudioBitrateKbps { get; set; }

        // ---- target ----
        [DataMember(Name = "mode", Order = 30)] public SendMode Mode { get; set; }
        [DataMember(Name = "host", Order = 31)] public string Host { get; set; } = "";
        [DataMember(Name = "port", Order = 32)] public int Port { get; set; }
        [DataMember(Name = "path", Order = 33)] public string StreamPath { get; set; } = "";
        [DataMember(Name = "user", Order = 34)] public string User { get; set; } = "";
        [DataMember(Name = "password", Order = 35)] public string Password { get; set; } = "";
        [DataMember(Name = "passphrase", Order = 36)] public string Passphrase { get; set; } = "";
        [DataMember(Name = "latencyMs", Order = 37)] public int LatencyMs { get; set; }
        [DataMember(Name = "overheadPercent", Order = 38)] public int OverheadPercent { get; set; }
        [DataMember(Name = "rateControl", Order = 39)] public bool RateControl { get; set; }

        // ---- tools / behaviour ----
        [DataMember(Name = "ffmpegPath", Order = 50)] public string FfmpegPath { get; set; } = "";
        [DataMember(Name = "autoRestart", Order = 51)] public bool AutoRestart { get; set; }

        void SetDefaults()
        {
            OutputIndex = 0;
            Fps = 60;
            Scale = ScaleMode.Native;
            HdrMode = HdrMode.Auto;
            Cursor = true;
            SdrWhite = CaptureArgs.DefaultSdrWhite;

            Encoder = VideoEncoder.HevcNvenc;
            Quality = QualityPreset.Balanced;
            BitrateMbps = 40;
            GopSeconds = 2.0;
            BufferSeconds = 2.0;
            BFrames = true;
            Lookahead = true;

            Audio = true;
            AudioDevice = "default";
            AudioCodec = AudioCodec.Aac;
            AudioBitrateKbps = 192;

            Mode = SendMode.Relay;
            Host = "";
            Port = 8890;
            StreamPath = "screen";
            User = "sender";
            Password = "";
            Passphrase = "";
            LatencyMs = RelayPublishLatencyMs;
            OverheadPercent = 25;
            RateControl = true;

            FfmpegPath = "";
            AutoRestart = false;
        }

        [OnDeserializing]
        void OnDeserializing(StreamingContext _) => SetDefaults();

        /// <summary>Clamps everything that came from a hand-edited file.</summary>
        public SenderSettings Normalize()
        {
            if (OutputIndex < 0) OutputIndex = 0;
            if (Fps < 1 || Fps > 480) Fps = 60;
            if (SdrWhite < 1 || SdrWhite > 10000) SdrWhite = CaptureArgs.DefaultSdrWhite;
            if (BitrateMbps < 1) BitrateMbps = 1;
            if (BitrateMbps > 500) BitrateMbps = 500;
            if (GopSeconds < 0.25) GopSeconds = 0.25;
            if (GopSeconds > 10) GopSeconds = 10;
            if (BufferSeconds < 0.5) BufferSeconds = 0.5;
            if (BufferSeconds > 10) BufferSeconds = 10;
            if (AudioBitrateKbps < 32) AudioBitrateKbps = 32;
            if (AudioBitrateKbps > 512) AudioBitrateKbps = 512;
            if (Port < 1 || Port > 65535) Port = 8890;
            if (LatencyMs < 20) LatencyMs = 20;
            if (LatencyMs > 30000) LatencyMs = 30000;
            if (OverheadPercent < 5) OverheadPercent = 5;
            if (OverheadPercent > 100) OverheadPercent = 100;
            if (string.IsNullOrWhiteSpace(StreamPath)) StreamPath = "screen";
            if (string.IsNullOrWhiteSpace(AudioDevice)) AudioDevice = "default";
            return this;
        }

        /// <summary>Output height for the selected scale mode (0 = native).</summary>
        public int ScaleHeight
        {
            get
            {
                switch (Scale)
                {
                    case ScaleMode.H1440: return 1440;
                    case ScaleMode.H1080: return 1080;
                    case ScaleMode.H720: return 720;
                    default: return 0;
                }
            }
        }

        /// <summary>
        /// Output size for a source of <paramref name="srcWidth"/>x<paramref name="srcHeight"/>:
        /// keeps the aspect ratio, rounds to even numbers and never upscales.
        /// </summary>
        public void ResolveSize(int srcWidth, int srcHeight, out int width, out int height)
        {
            width = srcWidth; height = srcHeight;
            int target = ScaleHeight;
            if (target <= 0 || srcHeight <= 0 || target >= srcHeight) { width = Even(srcWidth); height = Even(srcHeight); return; }
            double scale = (double)target / srcHeight;
            width = Even((int)Math.Round(srcWidth * scale));
            height = Even(target);
        }

        static int Even(int v) => v < 2 ? 2 : (v & 1) == 0 ? v : v - 1;

        /// <summary>Resolved capture pixel format for a monitor that is (or is not) in HDR mode.</summary>
        public CaptureFormat ResolveFormat(bool monitorIsHdr)
            => HdrMode == HdrMode.ForceSdr || !monitorIsHdr ? CaptureFormat.Nv12 : CaptureFormat.P010;

        public long BitrateBps => (long)BitrateMbps * 1_000_000L;

        public List<string> Validate()
        {
            var problems = new List<string>();
            if (Mode != SendMode.DirectListen && string.IsNullOrWhiteSpace(Host))
                problems.Add("Host is required.");
            if (Port < 1 || Port > 65535) problems.Add("Port must be 1..65535.");
            if (Mode == SendMode.Relay)
            {
                if (string.IsNullOrWhiteSpace(StreamPath)) problems.Add("Path is required in relay mode.");
                else if (StreamPath.IndexOf(':') >= 0) problems.Add("Path must not contain ':'.");
                if (User.IndexOf(':') >= 0) problems.Add("User must not contain ':'.");
                if (Password.IndexOf(':') >= 0) problems.Add("Password must not contain ':'.");
                if (string.IsNullOrEmpty(User) && !string.IsNullOrEmpty(Password)) problems.Add("A password needs a user name.");
            }
            if (!SrtUrl.IsValidPassphrase(Passphrase))
                problems.Add("Passphrase must be 10-79 characters (or empty for no encryption).");
            if (BitrateMbps < 1 || BitrateMbps > 500) problems.Add("Bitrate must be 1..500 Mbps.");
            if (Fps < 1 || Fps > 480) problems.Add("Frame rate must be 1..480.");
            if (LatencyMs < 20) problems.Add("Latency must be at least 20 ms.");

            EncoderInfo info = EncoderProfiles.Info(Encoder);
            if (Mode == SendMode.Relay && !info.MpegTsCompatible)
                problems.Add(info.DisplayName + " cannot go through the relay (AV1 has no MPEG-TS mapping). Use HEVC, or a direct connection.");
            return problems;
        }

        public Container ResolveContainer()
            => EncoderProfiles.Info(Encoder).MpegTsCompatible ? Container.MpegTs : Container.Matroska;

        public SrtUrlOptions ToSrtOptions()
        {
            return new SrtUrlOptions
            {
                Topology = Mode == SendMode.Relay ? SrtTopology.Relay
                         : Mode == SendMode.DirectListen ? SrtTopology.DirectListener
                         : SrtTopology.DirectCaller,
                Role = SrtRole.Publish,
                Host = Host,
                Port = Port,
                StreamPath = StreamPath,
                User = string.IsNullOrEmpty(User) ? null : User,
                Password = string.IsNullOrEmpty(Password) ? null : Password,
                Passphrase = string.IsNullOrEmpty(Passphrase) ? null : Passphrase,
                PbKeyLen = 32,
                LatencyMs = LatencyMs,
                BitrateBps = BitrateBps,
                RateControl = RateControl,
                OverheadPercent = OverheadPercent,
                ConnectTimeoutMs = 5000,
                IncludeRecvSocketBuffer = false,
            };
        }

        public EncoderSettings ToEncoderSettings(bool hdr, int masteringMaxNits)
        {
            return new EncoderSettings
            {
                Encoder = Encoder,
                Preset = Quality,
                Hdr = hdr,
                BitrateKbps = BitrateMbps * 1000,
                Fps = Fps,
                GopSeconds = GopSeconds,
                BufferSeconds = BufferSeconds,
                BFrames = BFrames,
                Lookahead = Lookahead,
                MasteringMaxNits = masteringMaxNits,
            };
        }

        /// <summary>Capture engine arguments for the selected monitor and size.</summary>
        public CaptureArgs ToCaptureArgs(OutputInfo output, int width, int height, CaptureFormat format,
                                         string videoPipe, string audioPipe, int parentPid)
        {
            return new CaptureArgs
            {
                OutputIndex = output.Index,
                Fps = Fps,
                Format = format,
                Width = width,
                Height = height,
                SdrWhite = (float)SdrWhite,
                Cursor = Cursor,
                VideoPipe = videoPipe,
                AudioPipe = audioPipe,
                Audio = Audio,
                AudioDevice = AudioDevice,
                ParentPid = parentPid,
            };
        }

        public static string DefaultPath => AppPaths.SettingsFile(FileName);

        public static SenderSettings Load(string path)
        {
            var s = Json.LoadFile<SenderSettings>(path);
            return (s ?? new SenderSettings()).Normalize();
        }

        public void Save(string path) => Json.SaveFile(path, this);

        public override string ToString()
            => string.Format(CultureInfo.InvariantCulture, "{0} {1} Mbps {2}fps -> {3}:{4}",
                             EncoderProfiles.Info(Encoder).FfmpegName, BitrateMbps, Fps, Host, Port);
    }
}
