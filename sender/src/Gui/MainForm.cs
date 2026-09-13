using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Globalization;
using System.Threading;
using System.Windows.Forms;
using HdrCast.Capture;
using HdrCast.Common;
using HdrCast.Sender.Capture;
using HdrCast.Sender.Model;

namespace HdrCast.Sender.Gui
{
    /// <summary>Code-only WinForms UI for the sender (design.md §2.7). UI strings are English.</summary>
    internal sealed class MainForm : Form
    {
        const string FfmpegExeName = "ffmpeg.exe";
        const int MaxLogLines = 500;
        const string FfmpegDownloadHint =
            "ffmpeg.exe was not found.\n\nDownload a Windows build that includes NVENC/AMF/QSV and libsrt " +
            "(gyan.dev \"full\" or BtbN win64-gpl), put ffmpeg.exe next to HdrCastSender.exe " +
            "(or in a tools\\ subfolder), or pick it with Browse.";

        readonly SenderSettings _settings;
        readonly string _settingsPath;
        readonly SenderRunner _runner = new SenderRunner();
        readonly ProcessJob _job = new ProcessJob();
        readonly List<string> _logLines = new List<string>();
        readonly Queue<string> _pendingLog = new Queue<string>();
        readonly System.Windows.Forms.Timer _uiTimer;
        List<OutputInfo> _outputs = new List<OutputInfo>();
        List<string> _audioDevices = new List<string>();
        bool _suppressEvents;
        bool _closing;
        Thread? _probeThread;

        // display
        ComboBox _output = null!;
        Button _refreshOutputs = null!;
        ComboBox _hdrMode = null!;
        Label _outputInfo = null!;

        // quality
        ComboBox _encoder = null!;
        Button _probe = null!;
        NumericUpDown _bitrate = null!;
        ComboBox _fps = null!;
        ComboBox _scale = null!;
        ComboBox _quality = null!;
        NumericUpDown _gop = null!;
        CheckBox _bframes = null!;
        CheckBox _lookahead = null!;

        // audio
        CheckBox _audio = null!;
        ComboBox _audioDevice = null!;
        ComboBox _audioCodec = null!;
        NumericUpDown _audioBitrate = null!;

        // target
        ComboBox _mode = null!;
        TextBox _host = null!;
        NumericUpDown _port = null!;
        TextBox _path = null!;
        TextBox _user = null!;
        TextBox _password = null!;
        TextBox _passphrase = null!;
        CheckBox _showSecrets = null!;
        ComboBox _region = null!;
        NumericUpDown _latency = null!;
        CheckBox _rateControl = null!;

        // tools / controls
        TextBox _ffmpegPath = null!;
        Label _ffmpegFound = null!;
        CheckBox _autoRestart = null!;
        Button _start = null!;
        Button _stop = null!;
        Button _copy = null!;
        Button _openLogs = null!;
        Button _viewerUrl = null!;

        // status
        Label _stState = null!;
        Label _stUptime = null!;
        Label _stCapture = null!;
        Label _stEncode = null!;
        Label _stBitrate = null!;
        Label _stSpeed = null!;
        Label _stDrops = null!;
        Label _stFormat = null!;
        TextBox _log = null!;

        public MainForm()
        {
            _settingsPath = SenderSettings.DefaultPath;
            _settings = SenderSettings.Load(_settingsPath);

            Text = "HdrCast Sender";
            AutoScaleMode = AutoScaleMode.Dpi;
            AutoScaleDimensions = new SizeF(96F, 96F);
            Font = SystemFonts.MessageBoxFont ?? Font;
            StartPosition = FormStartPosition.WindowsDefaultLocation;
            int em = Font.Height;
            ClientSize = new Size(em * 78, em * 58);
            MinimumSize = new Size(em * 64, em * 44);

            BuildLayout();
            LoadOutputs(false);
            LoadAudioDevices();
            ApplySettingsToUi();
            RefreshToolLabels();
            UpdateEnabledState();

            _runner.Job = _job;
            _runner.LogLine += OnRunnerLog;
            _runner.StateChanged += OnRunnerState;

            _uiTimer = new System.Windows.Forms.Timer { Interval = 300 };
            _uiTimer.Tick += (s, e) => UiTick();
            _uiTimer.Start();

            FormClosing += OnFormClosing;
            AppendLog("[app] settings: " + _settingsPath);
            AppendLog("[app] logs: " + AppPaths.LogsDir);
        }

        // ---------------------------------------------------------------- layout

        void BuildLayout()
        {
            var root = new TableLayoutPanel
            {
                Dock = DockStyle.Fill,
                ColumnCount = 2,
                RowCount = 4,
                Padding = new Padding(8),
            };
            root.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50));
            root.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50));
            root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            root.RowStyles.Add(new RowStyle(SizeType.Percent, 100));
            Controls.Add(root);

            root.Controls.Add(BuildCaptureGroup(), 0, 0);
            root.Controls.Add(BuildTargetGroup(), 1, 0);
            root.Controls.Add(BuildQualityGroup(), 0, 1);
            root.Controls.Add(BuildAudioGroup(), 1, 1);

            Control controls = BuildControlsPanel();
            root.Controls.Add(controls, 0, 2);
            root.SetColumnSpan(controls, 2);

            Control status = BuildStatusGroup();
            root.Controls.Add(status, 0, 3);
            root.SetColumnSpan(status, 2);
        }

        static GroupBox Group(string title)
        {
            return new GroupBox { Text = title, Dock = DockStyle.Fill, AutoSize = true, Padding = new Padding(8, 4, 8, 8) };
        }

        static TableLayoutPanel Grid(int columns)
        {
            var t = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = columns, AutoSize = true };
            for (int i = 0; i < columns; i++)
                t.ColumnStyles.Add(new ColumnStyle(i % 2 == 0 ? SizeType.AutoSize : SizeType.Percent, i % 2 == 0 ? 0 : 100));
            return t;
        }

        static Label Caption(string text) => new Label { Text = text, AutoSize = true, Anchor = AnchorStyles.Left, Margin = new Padding(3, 6, 3, 3) };

        Control BuildCaptureGroup()
        {
            GroupBox g = Group("Display");
            TableLayoutPanel t = Grid(2);

            _output = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _output.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) { OnOutputChanged(); } };
            _refreshOutputs = new Button { Text = "Refresh", AutoSize = true };
            _refreshOutputs.Click += (s, e) => LoadOutputs(true);

            var outputRow = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 2, AutoSize = true, Margin = new Padding(0) };
            outputRow.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
            outputRow.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            outputRow.Controls.Add(_output, 0, 0);
            outputRow.Controls.Add(_refreshOutputs, 1, 0);

            t.Controls.Add(Caption("Monitor"), 0, 0);
            t.Controls.Add(outputRow, 1, 0);

            _hdrMode = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _hdrMode.Items.AddRange(new object[] { "Auto (HDR when the monitor is in HDR mode)", "Force SDR (BT.709)" });
            _hdrMode.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) { _settings.HdrMode = (HdrMode)_hdrMode.SelectedIndex; UpdateOutputInfo(); } };
            t.Controls.Add(Caption("HDR"), 0, 1);
            t.Controls.Add(_hdrMode, 1, 1);

            _outputInfo = new Label { Dock = DockStyle.Fill, AutoSize = true, ForeColor = SystemColors.GrayText };
            t.Controls.Add(new Label { Text = "", AutoSize = true }, 0, 2);
            t.Controls.Add(_outputInfo, 1, 2);

            g.Controls.Add(t);
            return g;
        }

        Control BuildQualityGroup()
        {
            GroupBox g = Group("Picture");
            TableLayoutPanel t = Grid(2);

            _encoder = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            foreach (EncoderInfo info in EncoderProfiles.All) _encoder.Items.Add(info.DisplayName);
            _encoder.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) { _settings.Encoder = EncoderProfiles.All[_encoder.SelectedIndex].Id; UpdateEnabledState(); } };
            _probe = new Button { Text = "Test encoders", AutoSize = true };
            _probe.Click += (s, e) => StartProbe();
            var encRow = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 2, AutoSize = true, Margin = new Padding(0) };
            encRow.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
            encRow.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            encRow.Controls.Add(_encoder, 0, 0);
            encRow.Controls.Add(_probe, 1, 0);
            t.Controls.Add(Caption("Encoder"), 0, 0);
            t.Controls.Add(encRow, 1, 0);

            _bitrate = new NumericUpDown { Dock = DockStyle.Fill, Minimum = 1, Maximum = 500, Increment = 5 };
            _bitrate.ValueChanged += (s, e) => { if (!_suppressEvents) _settings.BitrateMbps = (int)_bitrate.Value; };
            t.Controls.Add(Caption("Bitrate (Mbps)"), 0, 1);
            t.Controls.Add(_bitrate, 1, 1);

            _fps = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _fps.Items.AddRange(new object[] { "30", "48", "60", "90", "120" });
            _fps.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) _settings.Fps = int.Parse((string)_fps.SelectedItem, CultureInfo.InvariantCulture); };
            t.Controls.Add(Caption("Frame rate"), 0, 2);
            t.Controls.Add(_fps, 1, 2);

            _scale = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _scale.Items.AddRange(new object[] { "Native", "1440p", "1080p", "720p" });
            _scale.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) { _settings.Scale = (ScaleMode)_scale.SelectedIndex; UpdateOutputInfo(); } };
            t.Controls.Add(Caption("Resolution"), 0, 3);
            t.Controls.Add(_scale, 1, 3);

            _quality = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _quality.Items.AddRange(new object[] { "Speed", "Balanced", "Quality (slowest preset, multipass)" });
            _quality.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) _settings.Quality = (QualityPreset)_quality.SelectedIndex; };
            t.Controls.Add(Caption("Quality preset"), 0, 4);
            t.Controls.Add(_quality, 1, 4);

            _gop = new NumericUpDown { Dock = DockStyle.Fill, Minimum = 1, Maximum = 10, DecimalPlaces = 1, Increment = 0.5M };
            _gop.ValueChanged += (s, e) => { if (!_suppressEvents) _settings.GopSeconds = (double)_gop.Value; };
            t.Controls.Add(Caption("Keyframe interval (s)"), 0, 5);
            t.Controls.Add(_gop, 1, 5);

            var flags = new FlowLayoutPanel { Dock = DockStyle.Fill, AutoSize = true, Margin = new Padding(0) };
            _bframes = new CheckBox { Text = "B-frames", AutoSize = true };
            _bframes.CheckedChanged += (s, e) => { if (!_suppressEvents) _settings.BFrames = _bframes.Checked; };
            _lookahead = new CheckBox { Text = "Lookahead", AutoSize = true };
            _lookahead.CheckedChanged += (s, e) => { if (!_suppressEvents) _settings.Lookahead = _lookahead.Checked; };
            flags.Controls.Add(_bframes);
            flags.Controls.Add(_lookahead);
            t.Controls.Add(Caption(""), 0, 6);
            t.Controls.Add(flags, 1, 6);

            g.Controls.Add(t);
            return g;
        }

        Control BuildAudioGroup()
        {
            GroupBox g = Group("Desktop audio");
            TableLayoutPanel t = Grid(2);

            _audio = new CheckBox { Text = "Capture desktop audio (WASAPI loopback)", AutoSize = true };
            _audio.CheckedChanged += (s, e) => { if (!_suppressEvents) { _settings.Audio = _audio.Checked; UpdateEnabledState(); } };
            t.Controls.Add(Caption(""), 0, 0);
            t.Controls.Add(_audio, 1, 0);

            _audioDevice = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _audioDevice.SelectedIndexChanged += (s, e) =>
            {
                if (_suppressEvents || _audioDevice.SelectedIndex < 0) return;
                _settings.AudioDevice = _audioDevice.SelectedIndex == 0 ? "default" : _audioDevices[_audioDevice.SelectedIndex - 1].Split('\t')[0];
            };
            t.Controls.Add(Caption("Device"), 0, 1);
            t.Controls.Add(_audioDevice, 1, 1);

            _audioCodec = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _audioCodec.Items.AddRange(new object[] { "AAC", "Opus" });
            _audioCodec.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) _settings.AudioCodec = _audioCodec.SelectedIndex == 1 ? AudioCodec.Opus : AudioCodec.Aac; };
            t.Controls.Add(Caption("Codec"), 0, 2);
            t.Controls.Add(_audioCodec, 1, 2);

            _audioBitrate = new NumericUpDown { Dock = DockStyle.Fill, Minimum = 32, Maximum = 512, Increment = 32 };
            _audioBitrate.ValueChanged += (s, e) => { if (!_suppressEvents) _settings.AudioBitrateKbps = (int)_audioBitrate.Value; };
            t.Controls.Add(Caption("Bitrate (kbps)"), 0, 3);
            t.Controls.Add(_audioBitrate, 1, 3);

            g.Controls.Add(t);
            return g;
        }

        Control BuildTargetGroup()
        {
            GroupBox g = Group("Where to send");
            TableLayoutPanel t = Grid(2);

            _mode = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            _mode.Items.AddRange(new object[]
            {
                "Relay (MediaMTX)",
                "Direct: I listen (needs a UDP port forward here)",
                "Direct: I dial the viewer",
            });
            _mode.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) { _settings.Mode = (SendMode)_mode.SelectedIndex; UpdateEnabledState(); } };
            t.Controls.Add(Caption("Mode"), 0, 0);
            t.Controls.Add(_mode, 1, 0);

            _host = new TextBox { Dock = DockStyle.Fill };
            _host.TextChanged += (s, e) => { if (!_suppressEvents) _settings.Host = _host.Text.Trim(); };
            t.Controls.Add(Caption("Host"), 0, 1);
            t.Controls.Add(_host, 1, 1);

            _port = new NumericUpDown { Dock = DockStyle.Fill, Minimum = 1, Maximum = 65535 };
            _port.ValueChanged += (s, e) => { if (!_suppressEvents) _settings.Port = (int)_port.Value; };
            t.Controls.Add(Caption("Port"), 0, 2);
            t.Controls.Add(_port, 1, 2);

            _path = new TextBox { Dock = DockStyle.Fill };
            _path.TextChanged += (s, e) => { if (!_suppressEvents) _settings.StreamPath = _path.Text.Trim(); };
            t.Controls.Add(Caption("Path"), 0, 3);
            t.Controls.Add(_path, 1, 3);

            _user = new TextBox { Dock = DockStyle.Fill };
            _user.TextChanged += (s, e) => { if (!_suppressEvents) _settings.User = _user.Text.Trim(); };
            t.Controls.Add(Caption("User"), 0, 4);
            t.Controls.Add(_user, 1, 4);

            _password = new TextBox { Dock = DockStyle.Fill, UseSystemPasswordChar = true };
            _password.TextChanged += (s, e) => { if (!_suppressEvents) _settings.Password = _password.Text; };
            t.Controls.Add(Caption("Password"), 0, 5);
            t.Controls.Add(_password, 1, 5);

            _passphrase = new TextBox { Dock = DockStyle.Fill, UseSystemPasswordChar = true };
            _passphrase.TextChanged += (s, e) => { if (!_suppressEvents) _settings.Passphrase = _passphrase.Text; };
            t.Controls.Add(Caption("Passphrase"), 0, 6);
            t.Controls.Add(_passphrase, 1, 6);

            _showSecrets = new CheckBox { Text = "Show password and passphrase", AutoSize = true };
            _showSecrets.CheckedChanged += (s, e) =>
            {
                _password.UseSystemPasswordChar = !_showSecrets.Checked;
                _passphrase.UseSystemPasswordChar = !_showSecrets.Checked;
            };
            t.Controls.Add(Caption(""), 0, 7);
            t.Controls.Add(_showSecrets, 1, 7);

            _region = new ComboBox { Dock = DockStyle.Fill, DropDownStyle = ComboBoxStyle.DropDownList };
            foreach (RegionPreset p in SenderSettings.RegionPresets) _region.Items.Add(p.ToString());
            _region.Items.Add("Custom");
            _region.SelectedIndexChanged += (s, e) =>
            {
                if (_suppressEvents || _region.SelectedIndex < 0 || _region.SelectedIndex >= SenderSettings.RegionPresets.Length) return;
                RegionPreset p = SenderSettings.RegionPresets[_region.SelectedIndex];
                _settings.LatencyMs = p.LatencyMs;
                _settings.OverheadPercent = p.OverheadPercent;
                _suppressEvents = true;
                _latency.Value = p.LatencyMs;
                _suppressEvents = false;
            };
            t.Controls.Add(Caption("Latency preset"), 0, 8);
            t.Controls.Add(_region, 1, 8);

            _latency = new NumericUpDown { Dock = DockStyle.Fill, Minimum = 20, Maximum = 30000, Increment = 100 };
            _latency.ValueChanged += (s, e) => { if (!_suppressEvents) { _settings.LatencyMs = (int)_latency.Value; SyncRegionCombo(); } };
            t.Controls.Add(Caption("SRT latency (ms)"), 0, 9);
            t.Controls.Add(_latency, 1, 9);

            _rateControl = new CheckBox { Text = "Pace the sender to the bitrate (maxbw / oheadbw)", AutoSize = true };
            _rateControl.CheckedChanged += (s, e) => { if (!_suppressEvents) _settings.RateControl = _rateControl.Checked; };
            t.Controls.Add(Caption(""), 0, 10);
            t.Controls.Add(_rateControl, 1, 10);

            g.Controls.Add(t);
            return g;
        }

        Control BuildControlsPanel()
        {
            var outer = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 2, AutoSize = true, Margin = new Padding(3, 6, 3, 3) };
            outer.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
            outer.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));

            var left = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 4, AutoSize = true, Margin = new Padding(0) };
            left.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            left.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
            left.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            left.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));

            _ffmpegPath = new TextBox { Dock = DockStyle.Fill };
            _ffmpegPath.TextChanged += (s, e) => { if (!_suppressEvents) { _settings.FfmpegPath = _ffmpegPath.Text.Trim(); RefreshToolLabels(); } };
            var browse = new Button { Text = "Browse…", AutoSize = true };
            browse.Click += (s, e) => BrowseFfmpeg();
            _ffmpegFound = new Label { AutoSize = true, Margin = new Padding(6, 6, 3, 3) };

            left.Controls.Add(Caption("ffmpeg.exe"), 0, 0);
            left.Controls.Add(_ffmpegPath, 1, 0);
            left.Controls.Add(browse, 2, 0);
            left.Controls.Add(_ffmpegFound, 3, 0);

            _autoRestart = new CheckBox { Text = "Restart automatically if the stream stops", AutoSize = true };
            _autoRestart.CheckedChanged += (s, e) => { if (!_suppressEvents) _settings.AutoRestart = _autoRestart.Checked; };
            left.Controls.Add(Caption(""), 0, 1);
            left.Controls.Add(_autoRestart, 1, 1);

            var buttons = new FlowLayoutPanel { AutoSize = true, FlowDirection = FlowDirection.LeftToRight, Margin = new Padding(0) };
            _start = new Button { Text = "Start", AutoSize = true, MinimumSize = new Size(Font.Height * 6, 0) };
            _start.Click += (s, e) => StartStream();
            _stop = new Button { Text = "Stop", AutoSize = true, MinimumSize = new Size(Font.Height * 6, 0), Enabled = false };
            _stop.Click += (s, e) => StopStream();
            _copy = new Button { Text = "Copy commands", AutoSize = true };
            _copy.Click += (s, e) => CopyCommands();
            _viewerUrl = new Button { Text = "Copy viewer URL", AutoSize = true };
            _viewerUrl.Click += (s, e) => CopyViewerUrl();
            _openLogs = new Button { Text = "Open logs", AutoSize = true };
            _openLogs.Click += (s, e) => OpenLogs();
            buttons.Controls.Add(_start);
            buttons.Controls.Add(_stop);
            buttons.Controls.Add(_copy);
            buttons.Controls.Add(_viewerUrl);
            buttons.Controls.Add(_openLogs);

            outer.Controls.Add(left, 0, 0);
            outer.Controls.Add(buttons, 1, 0);
            return outer;
        }

        Control BuildStatusGroup()
        {
            GroupBox g = Group("Status");
            var t = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 4, RowCount = 3 };
            t.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            t.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50));
            t.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            t.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50));
            t.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            t.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            t.RowStyles.Add(new RowStyle(SizeType.Percent, 100));

            _stState = Value();
            _stUptime = Value();
            _stCapture = Value();
            _stEncode = Value();
            _stBitrate = Value();
            _stSpeed = Value();
            _stDrops = Value();
            _stFormat = Value();

            var grid = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 8, AutoSize = true };
            for (int i = 0; i < 8; i++) grid.ColumnStyles.Add(new ColumnStyle(i % 2 == 0 ? SizeType.AutoSize : SizeType.Percent, i % 2 == 0 ? 0 : 25));
            grid.Controls.Add(Caption("State"), 0, 0); grid.Controls.Add(_stState, 1, 0);
            grid.Controls.Add(Caption("Uptime"), 2, 0); grid.Controls.Add(_stUptime, 3, 0);
            grid.Controls.Add(Caption("Capture"), 4, 0); grid.Controls.Add(_stCapture, 5, 0);
            grid.Controls.Add(Caption("Encode"), 6, 0); grid.Controls.Add(_stEncode, 7, 0);
            grid.Controls.Add(Caption("Bitrate"), 0, 1); grid.Controls.Add(_stBitrate, 1, 1);
            grid.Controls.Add(Caption("Speed"), 2, 1); grid.Controls.Add(_stSpeed, 3, 1);
            grid.Controls.Add(Caption("Dropped"), 4, 1); grid.Controls.Add(_stDrops, 5, 1);
            grid.Controls.Add(Caption("Format"), 6, 1); grid.Controls.Add(_stFormat, 7, 1);

            _log = new TextBox
            {
                Dock = DockStyle.Fill,
                Multiline = true,
                ReadOnly = true,
                ScrollBars = ScrollBars.Vertical,
                WordWrap = false,
                Font = new Font(FontFamily.GenericMonospace, Font.SizeInPoints - 0.5f),
            };

            var inner = new TableLayoutPanel { Dock = DockStyle.Fill, ColumnCount = 1, RowCount = 2 };
            inner.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            inner.RowStyles.Add(new RowStyle(SizeType.Percent, 100));
            inner.Controls.Add(grid, 0, 0);
            inner.Controls.Add(_log, 0, 1);
            g.Controls.Add(inner);
            return g;
        }

        static Label Value() => new Label { AutoSize = true, Anchor = AnchorStyles.Left, Margin = new Padding(3, 6, 12, 3), Text = "—" };

        // ---------------------------------------------------------------- settings <-> ui

        void ApplySettingsToUi()
        {
            _suppressEvents = true;
            try
            {
                SelectOutput(_settings.OutputIndex, _settings.OutputDeviceName, _settings.OutputFriendlyName);
                _hdrMode.SelectedIndex = (int)_settings.HdrMode;

                int encIdx = Array.FindIndex(EncoderProfiles.All, i => i.Id == _settings.Encoder);
                _encoder.SelectedIndex = Math.Max(0, encIdx);
                _bitrate.Value = Clamp(_settings.BitrateMbps, (int)_bitrate.Minimum, (int)_bitrate.Maximum);
                _fps.SelectedIndex = Math.Max(0, _fps.Items.IndexOf(_settings.Fps.ToString(CultureInfo.InvariantCulture)));
                _scale.SelectedIndex = (int)_settings.Scale;
                _quality.SelectedIndex = (int)_settings.Quality;
                _gop.Value = (decimal)Math.Max(1.0, Math.Min(10.0, _settings.GopSeconds));
                _bframes.Checked = _settings.BFrames;
                _lookahead.Checked = _settings.Lookahead;

                _audio.Checked = _settings.Audio;
                SelectAudioDevice(_settings.AudioDevice);
                _audioCodec.SelectedIndex = _settings.AudioCodec == AudioCodec.Opus ? 1 : 0;
                _audioBitrate.Value = Clamp(_settings.AudioBitrateKbps, (int)_audioBitrate.Minimum, (int)_audioBitrate.Maximum);

                _mode.SelectedIndex = (int)_settings.Mode;
                _host.Text = _settings.Host;
                _port.Value = Clamp(_settings.Port, 1, 65535);
                _path.Text = _settings.StreamPath;
                _user.Text = _settings.User;
                _password.Text = _settings.Password;
                _passphrase.Text = _settings.Passphrase;
                _latency.Value = Clamp(_settings.LatencyMs, (int)_latency.Minimum, (int)_latency.Maximum);
                _rateControl.Checked = _settings.RateControl;
                SyncRegionCombo();

                _ffmpegPath.Text = _settings.FfmpegPath;
                _autoRestart.Checked = _settings.AutoRestart;
            }
            finally { _suppressEvents = false; }
            UpdateOutputInfo();
        }

        static decimal Clamp(int value, int min, int max) => value < min ? min : value > max ? max : value;

        void SyncRegionCombo()
        {
            int idx = _region.Items.Count - 1;   // "Custom"
            for (int i = 0; i < SenderSettings.RegionPresets.Length; i++)
                if (SenderSettings.RegionPresets[i].LatencyMs == _settings.LatencyMs) { idx = i; break; }
            bool saved = _suppressEvents;
            _suppressEvents = true;
            _region.SelectedIndex = idx;
            _suppressEvents = saved;
        }

        void SaveSettings()
        {
            try
            {
                AppPaths.EnsureDir(AppPaths.ConfigDir);
                _settings.Normalize().Save(_settingsPath);
            }
            catch (Exception ex)
            {
                AppendLog("[app] could not save settings: " + ex.Message);
            }
        }

        // ---------------------------------------------------------------- outputs / audio devices

        void LoadOutputs(bool announce)
        {
            try
            {
                _outputs = OutputEnumerator.Enumerate();
            }
            catch (Exception ex)
            {
                _outputs = new List<OutputInfo>();
                AppendLog("[app] cannot enumerate displays: " + ex.Message);
            }
            bool saved = _suppressEvents;
            _suppressEvents = true;
            _output.Items.Clear();
            foreach (OutputInfo o in _outputs) _output.Items.Add(o.Label);
            if (_output.Items.Count > 0) _output.SelectedIndex = 0;
            _suppressEvents = saved;
            SelectOutput(_settings.OutputIndex, _settings.OutputDeviceName, _settings.OutputFriendlyName);
            if (announce) AppendLog("[app] found " + _outputs.Count + " display(s)");
            UpdateOutputInfo();
        }

        void SelectOutput(int index, string deviceName, string friendlyName)
        {
            if (_outputs.Count == 0) return;
            OutputInfo? match = OutputEnumerator.Resolve(_outputs, index, deviceName, friendlyName);
            if (match == null) return;
            bool saved = _suppressEvents;
            _suppressEvents = true;
            _output.SelectedIndex = _outputs.IndexOf(match);
            _suppressEvents = saved;
            _settings.OutputIndex = match.Index;
            _settings.OutputDeviceName = match.DeviceName;
            _settings.OutputFriendlyName = match.FriendlyName;
        }

        void OnOutputChanged()
        {
            OutputInfo? o = SelectedOutput;
            if (o == null) return;
            _settings.OutputIndex = o.Index;
            _settings.OutputDeviceName = o.DeviceName;
            _settings.OutputFriendlyName = o.FriendlyName;
            UpdateOutputInfo();
        }

        OutputInfo? SelectedOutput =>
            _output.SelectedIndex >= 0 && _output.SelectedIndex < _outputs.Count ? _outputs[_output.SelectedIndex] : null;

        void UpdateOutputInfo()
        {
            OutputInfo? o = SelectedOutput;
            if (o == null)
            {
                _outputInfo.Text = "No display found.";
                _stFormat.Text = "—";
                return;
            }
            bool hdr = o.Hdr && _settings.HdrMode == HdrMode.Auto;
            _settings.ResolveSize(o.Width, o.Height, out int w, out int h);
            string text = (hdr ? "HDR10 (PQ, BT.2020)" : "SDR (BT.709)") + "  " + w + "x" + h + " @ " + _settings.Fps + " fps";
            if (o.Hdr && _settings.HdrMode == HdrMode.ForceSdr) text += "  — the monitor is in HDR mode, streaming SDR anyway";
            if (!o.Hdr && _settings.HdrMode == HdrMode.Auto) text += "  — turn on HDR in Windows for an HDR stream";
            if (o.MaxLuminance > 0) text += "  (panel peak " + o.MaxLuminance.ToString("0", CultureInfo.InvariantCulture) + " nit)";
            _outputInfo.Text = text;
            _stFormat.Text = (hdr ? "p010 / HDR10" : "nv12 / SDR") + " " + w + "x" + h;
        }

        void LoadAudioDevices()
        {
            try
            {
                _audioDevices = new List<string>(AudioCapture.ListDevices());
            }
            catch (Exception)
            {
                _audioDevices = new List<string>();
            }
            bool saved = _suppressEvents;
            _suppressEvents = true;
            _audioDevice.Items.Clear();
            _audioDevice.Items.Add("System default");
            foreach (string d in _audioDevices)
            {
                string[] parts = d.Split('\t');
                _audioDevice.Items.Add(parts.Length > 1 ? parts[1] : parts[0]);
            }
            _audioDevice.SelectedIndex = 0;
            _suppressEvents = saved;
        }

        void SelectAudioDevice(string id)
        {
            if (string.IsNullOrEmpty(id) || id == "default") { _audioDevice.SelectedIndex = 0; return; }
            for (int i = 0; i < _audioDevices.Count; i++)
            {
                if (_audioDevices[i].StartsWith(id + "\t", StringComparison.OrdinalIgnoreCase)) { _audioDevice.SelectedIndex = i + 1; return; }
            }
            _audioDevice.SelectedIndex = 0;
        }

        // ---------------------------------------------------------------- plan / start / stop

        string? ResolveFfmpeg() => ToolLocator.Resolve(_settings.FfmpegPath, FfmpegExeName);

        void RefreshToolLabels()
        {
            string? path = ResolveFfmpeg();
            _ffmpegFound.Text = path != null ? "found" : "not found";
            _ffmpegFound.ForeColor = path != null ? Color.Green : Color.Firebrick;
        }

        void BrowseFfmpeg()
        {
            using (var dlg = new OpenFileDialog { Title = "Select ffmpeg.exe", Filter = "ffmpeg.exe|ffmpeg.exe|Programs (*.exe)|*.exe" })
            {
                if (dlg.ShowDialog(this) != DialogResult.OK) return;
                _ffmpegPath.Text = dlg.FileName;
            }
        }

        /// <summary>Builds the session plan, or shows why it cannot be built.</summary>
        SendPlan? BuildPlan(bool quiet)
        {
            List<string> problems = _settings.Validate();
            OutputInfo? output = SelectedOutput;
            if (output == null) problems.Add("No display selected.");
            string? ffmpeg = ResolveFfmpeg();
            if (ffmpeg == null) problems.Add(FfmpegDownloadHint);

            if (problems.Count > 0)
            {
                if (!quiet) MessageBox.Show(this, string.Join("\n\n", problems.ToArray()), "HdrCast",
                                            MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return null;
            }

            bool hdr = output!.Hdr && _settings.HdrMode == HdrMode.Auto;
            _settings.ResolveSize(output.Width, output.Height, out int w, out int h);
            CaptureFormat format = _settings.ResolveFormat(output.Hdr);

            int pid = Process.GetCurrentProcess().Id;
            string videoPipe = CaptureArgs.DefaultVideoPipe + "-" + pid.ToString(CultureInfo.InvariantCulture);
            string audioPipe = CaptureArgs.DefaultAudioPipe + "-" + pid.ToString(CultureInfo.InvariantCulture);

            CaptureArgs capture = _settings.ToCaptureArgs(output, w, h, format, videoPipe, audioPipe, pid);
            var captureArgv = new List<string> { "capture" };
            captureArgv.AddRange(capture.ToArgv());

            string url = SrtUrl.Build(_settings.ToSrtOptions());
            var plan = new FfmpegPlan
            {
                VideoPipeName = videoPipe,
                AudioPipeName = _settings.Audio ? audioPipe : null,
                PixelFormat = format,
                Width = w,
                Height = h,
                Fps = _settings.Fps,
                Encoder = _settings.ToEncoderSettings(hdr, (int)Math.Round(output.MaxLuminance)),
                Audio = _settings.Audio ? _settings.AudioCodec : AudioCodec.None,
                AudioBitrateKbps = _settings.AudioBitrateKbps,
                Container = _settings.ResolveContainer(),
                OutputUrl = url,
            };

            List<string> ffmpegArgv;
            try
            {
                ffmpegArgv = FfmpegCommandBuilder.Build(plan);
            }
            catch (Exception ex)
            {
                if (!quiet) MessageBox.Show(this, ex.Message, "HdrCast", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return null;
            }

            var pipeNames = new List<string> { videoPipe };
            if (_settings.Audio) pipeNames.Add(audioPipe);

            return new SendPlan
            {
                CaptureExe = Application.ExecutablePath,
                CaptureArgv = captureArgv.ToArray(),
                FfmpegExe = ffmpeg!,
                FfmpegArgv = ffmpegArgv.ToArray(),
                OutputUrl = url,
                Fps = _settings.Fps,
                PipeNames = pipeNames.ToArray(),
                Summary = EncoderProfiles.Info(_settings.Encoder).DisplayName + " " + _settings.BitrateMbps + " Mbps " +
                          w + "x" + h + "@" + _settings.Fps + (hdr ? " HDR10" : " SDR"),
            };
        }

        /// <summary>Stops off the UI thread: killing two children takes up to ~6 s and would freeze the window.</summary>
        void StopStream()
        {
            AppendLog("[app] stopping");
            _start.Enabled = false;
            _stop.Enabled = false;
            _runner.StopAsync(() => SafeInvoke(() =>
            {
                AppendLog("[app] stopped by user");
                UpdateEnabledState();
            }));
        }

        void StartStream()
        {
            SendPlan? plan = BuildPlan(false);
            if (plan == null) return;
            SaveSettings();
            AppendLog("[app] " + plan.Summary);
            _runner.Start(plan, _settings.AutoRestart);
            UpdateEnabledState();
        }

        void CopyCommands()
        {
            SendPlan? plan = BuildPlan(false);
            if (plan == null) return;
            string text =
                "REM Paste these into two command prompts (in this order).\r\n" +
                "REM In a .bat/.cmd FILE every % must be doubled (%% ) — at an interactive prompt paste as is.\r\n" +
                "REM 1) capture engine (creates the named pipes)\r\n" +
                ArgQuote.Quote(plan.CaptureExe) + " " + ArgQuote.Join(plan.CaptureArgv) + "\r\n\r\n" +
                "REM 2) ffmpeg (connects to the pipes and sends over SRT)\r\n" +
                ArgQuote.Quote(plan.FfmpegExe) + " " + ArgQuote.Join(plan.FfmpegArgv) + "\r\n";
            TrySetClipboard(text, "Commands copied to the clipboard.");
        }

        void CopyViewerUrl()
        {
            if (_settings.Mode != SendMode.Relay)
            {
                MessageBox.Show(this,
                    "A viewer URL can only be generated for relay mode.\n\n" +
                    "In direct mode the viewer connects to this PC's public address (or you dial the viewer), " +
                    "with the same passphrase — see docs/network.md.",
                    "HdrCast", MessageBoxButtons.OK, MessageBoxIcon.Information);
                return;
            }
            var options = _settings.ToSrtOptions();
            options.Role = SrtRole.Read;
            options.User = "viewer";
            options.Password = "<viewer password>";
            options.Passphrase = "<read passphrase>";
            options.LatencyMs = 1500;
            options.RateControl = false;
            options.IncludeRecvSocketBuffer = true;
            string url;
            try { url = SrtUrl.Build(options); }
            catch (Exception ex) { MessageBox.Show(this, ex.Message, "HdrCast", MessageBoxButtons.OK, MessageBoxIcon.Warning); return; }
            TrySetClipboard(url,
                "Viewer URL copied.\n\nReplace the placeholders with the viewer credentials from relay/setup.sh, " +
                "use the relay's public address, and set the latency for the viewer's region " +
                "(Japan 1500 / Australia 2000 / China 3500 ms).");
        }

        void TrySetClipboard(string text, string message)
        {
            try
            {
                Clipboard.SetText(text);
                MessageBox.Show(this, message, "HdrCast", MessageBoxButtons.OK, MessageBoxIcon.Information);
            }
            catch (Exception ex)
            {
                AppendLog("[app] clipboard failed: " + ex.Message);
            }
        }

        void OpenLogs()
        {
            try
            {
                AppPaths.EnsureDir(AppPaths.LogsDir);
                Process.Start(new ProcessStartInfo("explorer.exe", ArgQuote.Quote(AppPaths.LogsDir)) { UseShellExecute = true });
            }
            catch (Exception ex)
            {
                AppendLog("[app] cannot open the log folder: " + ex.Message);
            }
        }

        void StartProbe()
        {
            if (_probeThread != null && _probeThread.IsAlive) return;
            string? ffmpeg = ResolveFfmpeg();
            if (ffmpeg == null)
            {
                MessageBox.Show(this, FfmpegDownloadHint, "HdrCast", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return;
            }
            OutputInfo? o = SelectedOutput;
            bool hdr = o != null && o.Hdr && _settings.HdrMode == HdrMode.Auto;
            _probe.Enabled = false;
            AppendLog("[probe] testing encoders with " + ffmpeg + (hdr ? " (10-bit HDR)" : " (8-bit SDR)"));
            _probeThread = new Thread(() =>
            {
                List<EncoderProbeResult> results = EncoderProbe.ProbeAll(ffmpeg, hdr,
                    r => AppendLogThreadSafe("[probe] " + EncoderProfiles.Info(r.Encoder).FfmpegName + ": " +
                                             (r.Works ? "ok" : r.Message)));
                VideoEncoder? best = EncoderProbe.PickBest(results);
                SafeInvoke(() =>
                {
                    _probe.Enabled = true;
                    if (best.HasValue)
                    {
                        AppendLog("[probe] recommended: " + EncoderProfiles.Info(best.Value).DisplayName);
                        int idx = Array.FindIndex(EncoderProfiles.All, i => i.Id == best.Value);
                        if (idx >= 0) _encoder.SelectedIndex = idx;
                    }
                    else
                    {
                        AppendLog("[probe] no working encoder found — check the ffmpeg build and GPU drivers");
                    }
                });
            })
            { IsBackground = true, Name = "encoder-probe" };
            _probeThread.Start();
        }

        // ---------------------------------------------------------------- runtime

        void OnRunnerLog(string line)
        {
            line = SecretRedactor.Redact(line);
            lock (_pendingLog)
            {
                _pendingLog.Enqueue(line);
                if (_pendingLog.Count > 2000) _pendingLog.Dequeue();
            }
            SenderLog.Write(line);
        }

        void OnRunnerState(RunnerState state, string detail)
        {
            lock (_pendingLog) _pendingLog.Enqueue("[state] " + state + ": " + detail);
        }

        void UiTick()
        {
            lock (_pendingLog)
            {
                while (_pendingLog.Count > 0) AppendLog(_pendingLog.Dequeue());
            }

            _stState.Text = _runner.State == RunnerState.Idle ? "Idle" : _runner.State + " — " + _runner.StateDetail;
            _stState.ForeColor = _runner.State == RunnerState.Failed ? Color.Firebrick
                               : _runner.State == RunnerState.Streaming ? Color.Green : SystemColors.ControlText;

            if (_runner.IsRunning)
            {
                TimeSpan up = DateTime.UtcNow - _runner.StartedUtc;
                _stUptime.Text = up.TotalHours >= 1
                    ? ((int)up.TotalHours) + ":" + up.Minutes.ToString("00") + ":" + up.Seconds.ToString("00")
                    : up.Minutes.ToString("00") + ":" + up.Seconds.ToString("00");
            }
            else _stUptime.Text = "—";

            CaptureStats? cs = _runner.LastCaptureStats;
            _stCapture.Text = cs == null ? "—"
                : cs.Fps.ToString("0.0", CultureInfo.InvariantCulture) + " fps, " + cs.Duplicated + " dup, " +
                  cs.GpuMs.ToString("0.00", CultureInfo.InvariantCulture) + " ms GPU";

            FfmpegProgress? p = _runner.LastProgress;
            _stEncode.Text = p == null ? "—" : p.Fps.ToString("0.0", CultureInfo.InvariantCulture) + " fps, frame " + p.Frame;
            _stBitrate.Text = p == null || p.BitrateKbps < 0 ? "—" : (p.BitrateKbps / 1000.0).ToString("0.00", CultureInfo.InvariantCulture) + " Mbps";
            _stSpeed.Text = p == null || p.Speed < 0 ? "—" : p.Speed.ToString("0.00", CultureInfo.InvariantCulture) + "x";
            long captureDropped = cs?.Dropped ?? 0;
            long encodeDropped = p?.DropFrames ?? 0;
            _stDrops.Text = (captureDropped + encodeDropped) == 0 && cs == null ? "—"
                : captureDropped + " capture / " + encodeDropped + " encode";

            UpdateEnabledState();
        }

        void UpdateEnabledState()
        {
            bool running = _runner.IsRunning;
            _start.Enabled = !running;
            _stop.Enabled = running;
            _output.Enabled = !running;
            _refreshOutputs.Enabled = !running;
            _hdrMode.Enabled = !running;
            _encoder.Enabled = !running;
            _probe.Enabled = !running && (_probeThread == null || !_probeThread.IsAlive);
            _bitrate.Enabled = !running;
            _fps.Enabled = !running;
            _scale.Enabled = !running;
            _quality.Enabled = !running;
            _gop.Enabled = !running;
            _bframes.Enabled = !running;
            _lookahead.Enabled = !running;
            _audio.Enabled = !running;
            _audioDevice.Enabled = !running && _settings.Audio;
            _audioCodec.Enabled = !running && _settings.Audio;
            _audioBitrate.Enabled = !running && _settings.Audio;
            _mode.Enabled = !running;
            _host.Enabled = !running && _settings.Mode != SendMode.DirectListen;
            _port.Enabled = !running;
            bool relay = _settings.Mode == SendMode.Relay;
            _path.Enabled = !running && relay;
            _user.Enabled = !running && relay;
            _password.Enabled = !running && relay;
            _passphrase.Enabled = !running;
            _region.Enabled = !running;
            _latency.Enabled = !running;
            _rateControl.Enabled = !running;
            _ffmpegPath.Enabled = !running;
        }

        /// <summary>
        /// Marshals to the UI thread and does nothing once the form is going away. The encoder probe can run
        /// for a minute (7 trial encodes), so it routinely outlives the window it was started from; a plain
        /// BeginInvoke would throw ObjectDisposedException on a background thread and take the process down.
        /// </summary>
        void SafeInvoke(Action action)
        {
            if (_closing || IsDisposed || !IsHandleCreated) return;
            try
            {
                BeginInvoke(action);
            }
            catch (Exception)
            {
                // form is going away
            }
        }

        void AppendLogThreadSafe(string line)
        {
            lock (_pendingLog) _pendingLog.Enqueue(line);
        }

        void AppendLog(string line)
        {
            if (_closing) return;
            _logLines.Add(line);
            if (_logLines.Count > MaxLogLines) _logLines.RemoveRange(0, _logLines.Count - MaxLogLines);
            _log.Lines = _logLines.ToArray();
            _log.SelectionStart = _log.TextLength;
            _log.ScrollToCaret();
        }

        bool _stopRequestedOnClose;

        void OnFormClosing(object? sender, FormClosingEventArgs e)
        {
            _uiTimer.Stop();
            SaveSettings();

            if (_runner.IsRunning && !_stopRequestedOnClose)
            {
                // Killing the children takes seconds; close once they are gone instead of freezing the window.
                _stopRequestedOnClose = true;
                e.Cancel = true;
                Enabled = false;
                Text = "HdrCast Sender — stopping…";
                _runner.StopAsync(() => SafeInvoke(Close));
                return;
            }

            _closing = true;
            _runner.Stop();
            _job.Dispose();
        }
    }
}
