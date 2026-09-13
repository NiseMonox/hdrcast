using System;
using System.Collections.Generic;
using HdrCast.Common;
using HdrCast.Receiver;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>Defects found in review; each test fails again if the fix is undone.</summary>
    public class SecretRedactorTests
    {
        [Fact]
        public void Passphrase_is_masked_in_a_url()
        {
            string url = "srt://relay:8890?streamid=publish:screen&passphrase=SuperSecret123&pbkeylen=32&latency=800000";
            string safe = SecretRedactor.Redact(url);
            Assert.DoesNotContain("SuperSecret123", safe);
            Assert.Contains("passphrase=" + SecretRedactor.Mask, safe);
            Assert.Contains("latency=800000", safe);      // everything else is still readable
            Assert.Contains("pbkeylen=32", safe);
        }

        [Fact]
        public void Stream_id_password_is_masked_but_the_path_and_user_survive()
        {
            string url = "srt://relay:8890?streamid=publish:screen:sender:hunter2&latency=800000";
            string safe = SecretRedactor.Redact(url);
            Assert.DoesNotContain("hunter2", safe);
            Assert.Contains("publish:screen:sender:" + SecretRedactor.Mask, safe);
        }

        [Fact]
        public void Standard_haivision_stream_id_secret_is_masked()
        {
            string url = "srt://relay:8890?streamid=#!::m=publish,r=screen,u=sender,s=hunter2";
            string safe = SecretRedactor.Redact(url);
            Assert.DoesNotContain("hunter2", safe);
            Assert.Contains("u=sender", safe);
        }

        [Fact]
        public void A_whole_command_line_is_redacted()
        {
            string line = "[run] \"C:\\tools\\ffmpeg.exe\" -f mpegts \"srt://h:8890?streamid=publish:screen:sender:pw123&passphrase=abcdefghij\"";
            string safe = SecretRedactor.Redact(line);
            Assert.DoesNotContain("pw123", safe);
            Assert.DoesNotContain("abcdefghij", safe);
            Assert.Contains("ffmpeg.exe", safe);
        }

        [Fact]
        public void Text_without_secrets_is_unchanged()
        {
            const string line = "[ffmpeg] frame= 1234 fps=59.9 bitrate=39876.5kbits/s";
            Assert.Equal(line, SecretRedactor.Redact(line));
            Assert.Equal("", SecretRedactor.Redact(""));
            Assert.Equal("", SecretRedactor.Redact(null));
        }

        [Fact]
        public void A_stream_id_without_credentials_is_left_alone()
        {
            const string url = "srt://relay:8890?streamid=read:screen&latency=1500000";
            Assert.Equal(url, SecretRedactor.Redact(url));
        }
    }

    public class MpvExitClassificationTests
    {
        /// <summary>
        /// mpv prints the URL it failed to open, and our URL always contains the word "passphrase". Classifying
        /// the raw line made every failure look like an authentication failure, so a relay that simply had no
        /// publisher yet was reported to the viewer as a wrong password.
        /// </summary>
        [Fact]
        public void An_echoed_url_is_not_mistaken_for_an_auth_failure()
        {
            string[] lines =
            {
                "[ffmpeg] srt: Connection to srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000 failed: Input/output error",
                "Failed to open srt://127.0.0.1:8890?streamid=read:screen:viewer:pw&passphrase=abcdefghij1234&latency=1500000.",
            };
            Assert.NotEqual(MpvExitReason.AuthFailed, MpvLogClassifier.Classify(lines));
            Assert.Equal(MpvExitReason.ConnectionFailed, MpvLogClassifier.Classify(lines));
        }

        [Fact]
        public void A_relay_without_a_publisher_is_recognised_even_with_the_url_present()
        {
            string[] lines =
            {
                "[ffmpeg/demuxer] srt://h:8890?passphrase=abcdefghij: no stream is available on path 'screen'",
            };
            Assert.Equal(MpvExitReason.NoPublisher, MpvLogClassifier.Classify(lines));
            Assert.True(MpvLogClassifier.IsStreamUnavailable(lines[0]));
        }

        [Fact]
        public void A_real_passphrase_error_is_still_detected()
        {
            Assert.Equal(MpvExitReason.AuthFailed,
                MpvLogClassifier.ClassifyLine("[ffmpeg] srt: closed: invalid passphrase"));
            Assert.Equal(MpvExitReason.AuthFailed,
                MpvLogClassifier.ClassifyLine("[ffmpeg] srt: closed: passphrase not provided by client"));
            Assert.Equal(MpvExitReason.AuthFailed,
                MpvLogClassifier.ClassifyLine("[ffmpeg] srt: failed to authenticate: authentication failed"));
        }

        [Fact]
        public void Url_stripping_keeps_the_rest_of_the_line()
        {
            string stripped = MpvLogClassifier.StripUrls("Failed to open srt://h:8890?passphrase=x now.");
            Assert.DoesNotContain("passphrase", stripped);
            Assert.Contains("Failed to open", stripped);
            Assert.Contains("now.", stripped);
        }
    }

    public class ReceiverSettingsClampTests
    {
        /// <summary>mpv accepts "auto" or 10..10000 for --target-peak; 1..9 makes it refuse to start.</summary>
        [Theory]
        [InlineData(0, 0)]
        [InlineData(5, 0)]
        [InlineData(9, 0)]
        [InlineData(10, 10)]
        [InlineData(400, 400)]
        [InlineData(99999, 10000)]
        public void Target_peak_is_clamped_to_a_value_mpv_accepts(int input, int expected)
        {
            var s = new ReceiverSettings { TargetPeakNits = input };
            s.Normalize();
            Assert.Equal(expected, s.TargetPeakNits);
        }

        [Fact]
        public void A_clamped_target_peak_never_reaches_the_command_line_as_a_bad_value()
        {
            var s = new ReceiverSettings
            {
                Host = "h", DisplayMode = DisplayMode.SdrToneMap, TargetPeakNits = 5,
            }.Normalize();
            MpvLaunchPlan plan = MpvCommandBuilder.Build(s, 1, "mpv");
            foreach (string a in plan.MpvArgs)
                Assert.False(a.StartsWith("--target-peak=", StringComparison.Ordinal) && a != "--target-peak=auto",
                             "unexpected " + a);
        }
    }

    public class EncoderProbeErrorTests
    {
        /// <summary>
        /// ffmpeg's last line for a failed encode is the generic "Nothing was written into output file";
        /// the reason is earlier in the output, and that is what the GUI should show.
        /// </summary>
        [Fact]
        public void The_real_reason_is_preferred_over_the_generic_last_line()
        {
            const string output =
                "[hevc_nvenc @ 0x1] Cannot load nvEncodeAPI64.dll\n" +
                "[vost#0:0/hevc_nvenc @ 0x2] Could not open encoder before EOF\n" +
                "[out#0/null @ 0x3] Nothing was written into output file, because at least one of its streams received no packets.\n";
            string message = HdrCast.Sender.EncoderProbe.FirstError(output);
            Assert.Contains("nvEncodeAPI64", message);
            Assert.DoesNotContain("Nothing was written", message);
        }

        [Fact]
        public void Falls_back_to_the_last_line_when_nothing_matches()
        {
            Assert.Equal("something odd happened", HdrCast.Sender.EncoderProbe.FirstError("something odd happened\n\n"));
            Assert.Equal("failed", HdrCast.Sender.EncoderProbe.FirstError("\n\n"));
        }
    }

    public class ProcessJobTests
    {
        [Fact]
        public void Job_creation_never_throws_on_a_non_windows_host()
        {
            // The GUIs create one unconditionally; on Linux (where the tests run) it must degrade quietly.
            using (var job = new ProcessJob())
            {
                Assert.False(job.Assign(System.Diagnostics.Process.GetCurrentProcess()) && !job.IsAvailable);
            }
        }
    }
}
