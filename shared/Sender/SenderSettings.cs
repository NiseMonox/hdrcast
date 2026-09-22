using System;
using System.Collections.Generic;
using System.Globalization;
using System.Runtime.Serialization;
using HdrCast.Capture;
using HdrCast.Common;

namespace HdrCast.Sender.Model
{
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

    /// <summary>Persisted to %APPDATA%\HdrCast\sender.json via DataContractJsonSerializer.</summary>
    [DataContract]
    public sealed class SenderSettings
    {
        public const string FileName = "sender.json";

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

        // ---- group ----
        // The SRT settings of each viewer (latency, overhead, passphrase) come from the group key and that viewer's
        // join request; the old relay/direct fields (mode, host, port, …) are simply ignored in existing files.
        /// <summary>How many friends may watch at once (each one costs a full stream of upload).</summary>
        [DataMember(Name = "maxViewers", Order = 41)] public int MaxViewers { get; set; }
        /// <summary>This line's upload in Mbps, for the "upload needed" warning (0 = unknown).</summary>
        [DataMember(Name = "uploadMbps", Order = 42)] public int UploadMbps { get; set; }

        // ---- tools ----
        [DataMember(Name = "ffmpegPath", Order = 50)] public string FfmpegPath { get; set; } = "";

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

            MaxViewers = 4;
            UploadMbps = 0;

            FfmpegPath = "";
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
            if (string.IsNullOrWhiteSpace(AudioDevice)) AudioDevice = "default";
            // 1, 3 and 6 were the AV1 encoders in older files
            if (!Enum.IsDefined(typeof(VideoEncoder), Encoder)) Encoder = VideoEncoder.HevcNvenc;
            if (MaxViewers < 1) MaxViewers = 1;
            if (MaxViewers > 16) MaxViewers = 16;
            if (UploadMbps < 0) UploadMbps = 0;
            if (UploadMbps > 100000) UploadMbps = 100000;
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
            if (BitrateMbps < 1 || BitrateMbps > 500) problems.Add("Bitrate must be 1..500 Mbps.");
            if (Fps < 1 || Fps > 480) problems.Add("Frame rate must be 1..480.");
            if (!Enum.IsDefined(typeof(VideoEncoder), Encoder)) problems.Add("Pick an encoder.");
            return problems;
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
            => string.Format(CultureInfo.InvariantCulture, "{0} {1} Mbps {2}fps, up to {3} viewers",
                             EncoderProfiles.Info(Encoder).FfmpegName, BitrateMbps, Fps, MaxViewers);
    }
}
