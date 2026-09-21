using System;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Net.Sockets;
using HdrCast.Common;

namespace HdrCast.P2p
{
    /// <summary>
    /// One viewer's ffmpeg: reads the shared MPEG-TS on stdin, copies it (no re-encode) and serves it as an SRT
    /// listener on 127.0.0.1 with that viewer's latency, overhead and the group's passphrase (docs/design.md §8.8).
    /// libsrt does the retransmission and pacing per viewer; <see cref="HostLink"/> carries its datagrams.
    /// </summary>
    public sealed class RemuxChild : IDisposable
    {
        readonly LineProcess _process;

        public RemuxChild(string ffmpegPath, string srtUrl, ProcessJob? job = null)
        {
            SrtUrl = srtUrl;
            _process = new LineProcess(ffmpegPath, ArgQuote.Join(BuildArguments(srtUrl)))
            {
                RedirectStdin = true,
                Job = job,
            };
            _process.StderrLine += line => Log?.Invoke(SecretRedactor.Redact(line));
            _process.Exited += code => Exited?.Invoke(code);
        }

        public string SrtUrl { get; }
        /// <summary>ffmpeg's stderr lines, already redacted.</summary>
        public event Action<string>? Log;
        public event Action<int>? Exited;

        public Stream Input => _process.StdinStream ?? throw new InvalidOperationException("not started");
        public bool HasExited => _process.HasExited;
        public string SafeCommandLine => _process.SafeCommandLine;

        public void Start() => _process.Start();

        /// <summary>
        /// Small probe (the input is already clean MPEG-TS), copy every stream, keep repeating the parameter sets
        /// for a viewer that joins mid-stream, no mux delay.
        /// </summary>
        public static List<string> BuildArguments(string srtUrl) => new List<string>
        {
            "-hide_banner", "-loglevel", "warning",
            "-probesize", "65536", "-analyzeduration", "500000",
            "-f", "mpegts", "-i", "pipe:0",
            "-map", "0", "-c", "copy",
            "-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0",
            srtUrl,
        };

        /// <summary>The per-viewer listener URL: 127.0.0.1 only; the WAN side is <see cref="HostLink"/>'s job.</summary>
        public static string BuildListenerUrl(int port, string passphrase, int latencyMs, int overheadPercent, long bitrateBps) =>
            HdrCast.Common.SrtUrl.Build(new SrtUrlOptions
            {
                Topology = SrtTopology.DirectListener,
                ListenAddress = "127.0.0.1",
                Port = port,
                Role = SrtRole.Publish,
                Passphrase = passphrase,
                PbKeyLen = 32,
                LatencyMs = latencyMs,
                BitrateBps = bitrateBps,
                RateControl = true,
                OverheadPercent = Math.Max(5, Math.Min(100, overheadPercent)),
            });

        /// <summary>A UDP port that is free on 127.0.0.1 right now (the child binds it a moment later).</summary>
        public static int FreeLoopbackUdpPort()
        {
            using (var s = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
            {
                s.Bind(new IPEndPoint(IPAddress.Loopback, 0));
                return ((IPEndPoint)s.LocalEndPoint!).Port;
            }
        }

        public void Dispose()
        {
            // Kill first: a child still waiting for its viewer is not reading stdin, and closing a full pipe blocks
            // until someone reads it — which would be never.
            Stream? stdin = _process.StdinStream;
            _process.Dispose();
            try { stdin?.Dispose(); } catch (Exception) { }
        }
    }
}
