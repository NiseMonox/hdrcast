using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Globalization;
using System.Threading;
using System.Windows.Forms;
using HdrCast.Common;

namespace HdrCast.Receiver.Gui
{
    /// <summary>Code-only WinForms UI for the receiver (design.md section 3). UI strings are English.</summary>
    internal sealed class MainForm : Form
    {
        private const string MpvExeName = "mpv.exe";
        private const string FfmpegExeName = "ffmpeg.exe";
        private const int MaxLogLines = 500;
        private const string MpvDownloadHint =
            "mpv.exe was not found.\n\n" +
            "Easiest fix: run scripts\\get-tools.cmd viewer (double-click get-tools.cmd). It downloads an mpv " +
            "build with libsrt into the tools\\ folder, then checks it.\n\n" +
            "By hand: take a shinchiro or zhongfly mpv-winbuild and put mpv.exe next to HdrCastReceiver.exe " +
            "(or in tools\\), or pick it with Browse.";

        private readonly ReceiverSettings _settings;
        private readonly string _settingsPath;
        private readonly MpvRunner _runner = new MpvRunner();
        private readonly ProcessJob _job = new ProcessJob();
        private readonly List<string> _logLines = new List<string>();
        private readonly Queue<string> _pendingLog = new Queue<string>();
        private readonly System.Windows.Forms.Timer _uiTimer;
        private bool _suppressEvents;
        private bool _closing;

        // connection
        private ComboBox _mode = null!;
        private TextBox _host = null!;
        private NumericUpDown _port = null!;
        private TextBox _path = null!;
        private TextBox _user = null!;
        private TextBox _password = null!;
        private TextBox _passphrase = null!;
        private CheckBox _showSecrets = null!;
        private ComboBox _latencyPreset = null!;
        private NumericUpDown _latency = null!;
        private NumericUpDown _bitrate = null!;

        // playback
        private ComboBox _displayMode = null!;
        private ComboBox _toneMapping = null!;
        private NumericUpDown _targetPeak = null!;
        private ComboBox _hwdec = null!;
        private NumericUpDown _buffer = null!;
        private CheckBox _fullscreen = null!;
        private NumericUpDown _screen = null!;
        private ComboBox _frontend = null!;
        private TextBox _mpvPath = null!;
        private Label _mpvFound = null!;
        private TextBox _ffmpegPath = null!;
        private Label _ffmpegFound = null!;
        private CheckBox _catchUp = null!;

        // controls
        private Button _play = null!;
        private Button _stop = null!;
        private Button _fsToggle = null!;
        private Button _copy = null!;
        private Button _openLogs = null!;

        // status
        private Label _stState = null!;
        private Label _stUptime = null!;
        private Label _stCache = null!;
        private Label _stDrops = null!;
        private Label _stFps = null!;
        private Label _stBitrate = null!;
        private Label _stHwdec = null!;
        private Label _stSource = null!;
        private Label _stTarget = null!;
        private Label _stOutput = null!;
        private TextBox _log = null!;

        public MainForm()
        {
            _settingsPath = ReceiverSettings.DefaultPath;
            _settings = ReceiverSettings.Load(_settingsPath);

            Text = "HdrCast Receiver";
            AutoScaleMode = AutoScaleMode.Dpi;
            AutoScaleDimensions = new SizeF(96F, 96F);
            Font = SystemFonts.MessageBoxFont ?? Font;
            StartPosition = FormStartPosition.WindowsDefaultLocation;
            int em = Font.Height;
            ClientSize = new Size(em * 74, em * 56);
            MinimumSize = new Size(em * 60, em * 40);

            BuildLayout();
            ApplySettingsToUi();
            RefreshToolLabels();
            UpdateEnabledState();

            _runner.Job = _job;
            _runner.LogLine += OnRunnerLog;
            _runner.StateChanged += OnRunnerState;
            _runner.StatusChanged += () => { /* polled by the UI timer */ };

            _uiTimer = new System.Windows.Forms.Timer { Interval = 300 };
            _uiTimer.Tick += (s, e) => UiTick();
            _uiTimer.Start();

            FormClosing += OnFormClosing;
            AppendLog("[app] settings: " + _settingsPath);
            AppendLog("[app] logs: " + AppPaths.LogsDir);
        }

        // ---- layout ---------------------------------------------------------------------------

        private void BuildLayout()
        {
            int em = Font.Height;
            var root = new TableLayoutPanel
            {
                Dock = DockStyle.Fill,
                ColumnCount = 1,
                RowCount = 4,
                Padding = new Padding(em / 2),
            };
            root.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100F));
            root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            root.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            root.RowStyles.Add(new RowStyle(SizeType.Percent, 100F));

            var settingsRow = new TableLayoutPanel
            {
                Dock = DockStyle.Top,
                AutoSize = true,
                AutoSizeMode = AutoSizeMode.GrowAndShrink,
                ColumnCount = 2,
                RowCount = 1,
            };
            settingsRow.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50F));
            settingsRow.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50F));
            settingsRow.Controls.Add(BuildConnectionGroup(), 0, 0);
            settingsRow.Controls.Add(BuildPlaybackGroup(), 1, 0);

            root.Controls.Add(settingsRow, 0, 0);
            root.Controls.Add(BuildButtons(), 0, 1);
            root.Controls.Add(BuildStatusGroup(), 0, 2);
            root.Controls.Add(BuildLogGroup(), 0, 3);
            Controls.Add(root);
        }

        private static TableLayoutPanel NewGrid()
        {
            var grid = new TableLayoutPanel
            {
                Dock = DockStyle.Fill,
                AutoSize = true,
                AutoSizeMode = AutoSizeMode.GrowAndShrink,
                ColumnCount = 2,
            };
            grid.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            grid.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100F));
            return grid;
        }

        private void AddRow(TableLayoutPanel grid, string label, Control control)
        {
            int row = grid.RowCount++;
            grid.RowStyles.Add(new RowStyle(SizeType.AutoSize));
            var l = new Label
            {
                Text = label,
                AutoSize = true,
                Anchor = AnchorStyles.Left,
                Margin = new Padding(3, 6, 6, 3),
            };
            grid.Controls.Add(l, 0, row);
            grid.Controls.Add(control, 1, row);
        }

        private FlowLayoutPanel Row(params Control[] controls)
        {
            var p = new FlowLayoutPanel
            {
                AutoSize = true,
                AutoSizeMode = AutoSizeMode.GrowAndShrink,
                FlowDirection = FlowDirection.LeftToRight,
                WrapContents = false,
                Margin = new Padding(0),
                Anchor = AnchorStyles.Left | AnchorStyles.Right,
            };
            foreach (var c in controls) p.Controls.Add(c);
            return p;
        }

        private TextBox NewText(int widthEm)
        {
            return new TextBox { Width = Font.Height * widthEm, Anchor = AnchorStyles.Left | AnchorStyles.Right };
        }

        private ComboBox NewCombo(int widthEm, params string[] items)
        {
            var c = new ComboBox { DropDownStyle = ComboBoxStyle.DropDownList, Width = Font.Height * widthEm };
            c.Items.AddRange(items);
            if (items.Length > 0) c.SelectedIndex = 0;
            return c;
        }

        private NumericUpDown NewNumber(int widthEm, decimal min, decimal max, decimal value, int decimals = 0, decimal step = 1)
        {
            var n = new NumericUpDown
            {
                Width = Font.Height * widthEm,
                Minimum = min,
                Maximum = max,
                DecimalPlaces = decimals,
                Increment = step,
                Value = Math.Min(Math.Max(value, min), max),
            };
            return n;
        }

        private static Label Small(string text) => new Label { Text = text, AutoSize = true, Anchor = AnchorStyles.Left, Margin = new Padding(6, 6, 3, 3) };

        private GroupBox BuildConnectionGroup()
        {
            var group = new GroupBox { Text = "Connection", Dock = DockStyle.Fill, AutoSize = true, AutoSizeMode = AutoSizeMode.GrowAndShrink, Padding = new Padding(8) };
            var grid = NewGrid();

            _mode = NewCombo(22, "Relay (MediaMTX)", "Direct: I listen (port forward needed)", "Direct: I dial the sender");
            _mode.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) UpdateEnabledState(); };
            AddRow(grid, "Mode", _mode);

            _host = NewText(16);
            _port = NewNumber(6, 1, 65535, 8890);
            AddRow(grid, "Host", Row(_host, Small("Port"), _port));

            _path = NewText(10);
            AddRow(grid, "Path", _path);

            _user = NewText(10);
            _password = NewText(12);
            _password.UseSystemPasswordChar = true;
            AddRow(grid, "User", Row(_user, Small("Password"), _password));

            _passphrase = NewText(22);
            _passphrase.UseSystemPasswordChar = true;
            _showSecrets = new CheckBox { Text = "Show", AutoSize = true, Margin = new Padding(6, 4, 3, 3) };
            _showSecrets.CheckedChanged += (s, e) =>
            {
                _password.UseSystemPasswordChar = !_showSecrets.Checked;
                _passphrase.UseSystemPasswordChar = !_showSecrets.Checked;
            };
            AddRow(grid, "Passphrase", Row(_passphrase, _showSecrets));

            var presetNames = new List<string>();
            foreach (var p in ReceiverSettings.LatencyPresets) presetNames.Add(p.ToString());
            presetNames.Add("Custom");
            _latencyPreset = NewCombo(12, presetNames.ToArray());
            _latency = NewNumber(7, 20, 30000, 1500, 0, 100);
            _latencyPreset.SelectedIndexChanged += (s, e) =>
            {
                if (_suppressEvents) return;
                int i = _latencyPreset.SelectedIndex;
                if (i >= 0 && i < ReceiverSettings.LatencyPresets.Length)
                {
                    _suppressEvents = true;
                    _latency.Value = ReceiverSettings.LatencyPresets[i].LatencyMs;
                    _suppressEvents = false;
                }
            };
            _latency.ValueChanged += (s, e) => { if (!_suppressEvents) SyncLatencyPreset(); };
            AddRow(grid, "Latency", Row(_latencyPreset, _latency, Small("ms")));

            _bitrate = NewNumber(7, 1, 1000, 100);
            AddRow(grid, "Expected bitrate", Row(_bitrate, Small("Mbps (sizes the SRT receive buffer; larger is safer)")));

            group.Controls.Add(grid);
            return group;
        }

        private GroupBox BuildPlaybackGroup()
        {
            var group = new GroupBox { Text = "Playback", Dock = DockStyle.Fill, AutoSize = true, AutoSizeMode = AutoSizeMode.GrowAndShrink, Padding = new Padding(8) };
            var grid = NewGrid();

            _displayMode = NewCombo(18, "Auto (follow Windows HDR)", "HDR passthrough (Windows HDR on)", "SDR tone-map");
            _displayMode.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) UpdateEnabledState(); };
            _toneMapping = NewCombo(8, ReceiverSettings.ToneMappingAlgorithms);
            AddRow(grid, "Display mode", Row(_displayMode, _toneMapping));

            _targetPeak = NewNumber(6, 0, 10000, 0, 0, 50);
            AddRow(grid, "Target peak", Row(_targetPeak, Small("nit (0 = auto; set if Windows reports a bogus peak)")));

            _hwdec = NewCombo(12, "auto-safe", "d3d11va", "d3d11va-copy", "no (software)");
            AddRow(grid, "HW decode", _hwdec);

            _buffer = NewNumber(6, 0.2m, 30m, 1.5m, 1, 0.5m);
            AddRow(grid, "Buffer", Row(_buffer, Small("s (prebuffer before playback starts)")));

            _fullscreen = new CheckBox { Text = "Fullscreen on start", AutoSize = true, Margin = new Padding(3, 4, 3, 3) };
            _screen = NewNumber(5, 0, 32, 0);
            AddRow(grid, "Window", Row(_fullscreen, Small("Monitor"), _screen));

            _frontend = NewCombo(18, "mpv direct (srt:// via libsrt)", "ffmpeg pipe → mpv (fallback)");
            _frontend.SelectedIndexChanged += (s, e) => { if (!_suppressEvents) UpdateEnabledState(); };
            AddRow(grid, "Frontend", _frontend);

            _mpvPath = NewText(16);
            _mpvPath.TextChanged += (s, e) => RefreshToolLabels();
            var mpvBrowse = new Button { Text = "Browse…", AutoSize = true };
            mpvBrowse.Click += (s, e) => Browse(_mpvPath, MpvExeName);
            var mpvAuto = new Button { Text = "Auto", AutoSize = true };
            mpvAuto.Click += (s, e) => { _mpvPath.Text = ""; RefreshToolLabels(); };
            AddRow(grid, "mpv path", Row(_mpvPath, mpvBrowse, mpvAuto));
            _mpvFound = new Label { AutoSize = true, ForeColor = SystemColors.GrayText, Margin = new Padding(3, 0, 3, 3) };
            AddRow(grid, "", _mpvFound);

            _ffmpegPath = NewText(16);
            _ffmpegPath.TextChanged += (s, e) => RefreshToolLabels();
            var ffBrowse = new Button { Text = "Browse…", AutoSize = true };
            ffBrowse.Click += (s, e) => Browse(_ffmpegPath, FfmpegExeName);
            var ffAuto = new Button { Text = "Auto", AutoSize = true };
            ffAuto.Click += (s, e) => { _ffmpegPath.Text = ""; RefreshToolLabels(); };
            AddRow(grid, "ffmpeg path", Row(_ffmpegPath, ffBrowse, ffAuto));
            _ffmpegFound = new Label { AutoSize = true, ForeColor = SystemColors.GrayText, Margin = new Padding(3, 0, 3, 3) };
            AddRow(grid, "", _ffmpegFound);

            _catchUp = new CheckBox { Text = "Auto catch-up (1.05x when the buffer runs 1.5 s over target)", AutoSize = true, Margin = new Padding(3, 4, 3, 3) };
            AddRow(grid, "", _catchUp);

            group.Controls.Add(grid);
            return group;
        }

        private Control BuildButtons()
        {
            var p = new FlowLayoutPanel
            {
                Dock = DockStyle.Top,
                AutoSize = true,
                AutoSizeMode = AutoSizeMode.GrowAndShrink,
                FlowDirection = FlowDirection.LeftToRight,
                Padding = new Padding(0, 4, 0, 4),
            };
            _play = new Button { Text = "Play", AutoSize = true, Padding = new Padding(8, 2, 8, 2) };
            _play.Click += (s, e) => Play();
            _stop = new Button { Text = "Stop", AutoSize = true, Padding = new Padding(8, 2, 8, 2) };
            _stop.Click += (s, e) => StopAsync();
            _fsToggle = new Button { Text = "Toggle fullscreen", AutoSize = true };
            _fsToggle.Click += (s, e) => _runner.ToggleFullscreen();
            _copy = new Button { Text = "Copy command", AutoSize = true };
            _copy.Click += (s, e) => CopyCommand();
            _openLogs = new Button { Text = "Open logs", AutoSize = true };
            _openLogs.Click += (s, e) => OpenLogs();
            p.Controls.AddRange(new Control[] { _play, _stop, _fsToggle, _copy, _openLogs });
            return p;
        }

        private GroupBox BuildStatusGroup()
        {
            var group = new GroupBox { Text = "Status", Dock = DockStyle.Top, AutoSize = true, AutoSizeMode = AutoSizeMode.GrowAndShrink, Padding = new Padding(8) };
            var grid = new TableLayoutPanel
            {
                Dock = DockStyle.Fill,
                AutoSize = true,
                AutoSizeMode = AutoSizeMode.GrowAndShrink,
                ColumnCount = 4,
            };
            grid.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            grid.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50F));
            grid.ColumnStyles.Add(new ColumnStyle(SizeType.AutoSize));
            grid.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 50F));

            Label V() => new Label { Text = "-", AutoSize = true, Anchor = AnchorStyles.Left, Margin = new Padding(3, 4, 3, 3) };
            Label K(string t) => new Label { Text = t, AutoSize = true, Anchor = AnchorStyles.Left, ForeColor = SystemColors.GrayText, Margin = new Padding(3, 4, 6, 3) };

            _stState = V();
            _stState.Font = new Font(Font, FontStyle.Bold);
            _stUptime = V();
            _stCache = V();
            _stDrops = V();
            _stFps = V();
            _stBitrate = V();
            _stHwdec = V();
            _stOutput = V();
            _stSource = V();
            _stTarget = V();

            int row = 0;
            void Pair(string k1, Label v1, string k2, Label v2)
            {
                grid.RowStyles.Add(new RowStyle(SizeType.AutoSize));
                grid.Controls.Add(K(k1), 0, row);
                grid.Controls.Add(v1, 1, row);
                grid.Controls.Add(K(k2), 2, row);
                grid.Controls.Add(v2, 3, row);
                row++;
            }
            void Single(string k, Label v)
            {
                grid.RowStyles.Add(new RowStyle(SizeType.AutoSize));
                grid.Controls.Add(K(k), 0, row);
                grid.Controls.Add(v, 1, row);
                grid.SetColumnSpan(v, 3);
                row++;
            }
            Single("State", _stState);
            Pair("Uptime", _stUptime, "Cache", _stCache);
            Pair("Dropped frames", _stDrops, "FPS", _stFps);
            Pair("Video bitrate", _stBitrate, "HW decoder", _stHwdec);
            Single("Source", _stSource);
            Single("Target", _stTarget);
            Single("Output", _stOutput);
            grid.RowCount = row;
            group.Controls.Add(grid);
            return group;
        }

        private GroupBox BuildLogGroup()
        {
            var group = new GroupBox { Text = "mpv log", Dock = DockStyle.Fill, Padding = new Padding(8) };
            _log = new TextBox
            {
                Dock = DockStyle.Fill,
                Multiline = true,
                ReadOnly = true,
                ScrollBars = ScrollBars.Both,
                WordWrap = false,
                Font = new Font(FontFamily.GenericMonospace, Font.Size),
                BackColor = SystemColors.Window,
            };
            group.Controls.Add(_log);
            return group;
        }

        // ---- settings <-> UI --------------------------------------------------------------

        private void ApplySettingsToUi()
        {
            _suppressEvents = true;
            try
            {
                var s = _settings;
                _mode.SelectedIndex = (int)s.Mode;
                _host.Text = s.Host;
                _port.Value = Clamp(s.Port, _port);
                _path.Text = s.StreamPath;
                _user.Text = s.User;
                _password.Text = s.Password;
                _passphrase.Text = s.Passphrase;
                _latency.Value = Clamp(s.LatencyMs, _latency);
                SyncLatencyPreset();
                _bitrate.Value = Clamp(s.ExpectedBitrateMbps, _bitrate);
                _displayMode.SelectedIndex = (int)s.DisplayMode;
                int tm = Array.IndexOf(ReceiverSettings.ToneMappingAlgorithms, s.ToneMapping);
                _toneMapping.SelectedIndex = tm >= 0 ? tm : 0;
                _targetPeak.Value = Clamp(s.TargetPeakNits, _targetPeak);
                _hwdec.SelectedIndex = (int)s.HwDec;
                _buffer.Value = Clamp((decimal)s.BufferSeconds, _buffer);
                _fullscreen.Checked = s.Fullscreen;
                _screen.Value = Clamp(s.FullscreenScreen, _screen);
                _frontend.SelectedIndex = (int)s.Frontend;
                _mpvPath.Text = s.MpvPath;
                _ffmpegPath.Text = s.FfmpegPath;
                _catchUp.Checked = s.AutoCatchUp;
            }
            finally
            {
                _suppressEvents = false;
            }
        }

        private ReceiverSettings ReadUiIntoSettings()
        {
            var s = _settings;
            s.Mode = (ConnectionMode)Math.Max(0, _mode.SelectedIndex);
            s.Host = _host.Text.Trim();
            s.Port = (int)_port.Value;
            s.StreamPath = _path.Text.Trim();
            s.User = _user.Text.Trim();
            s.Password = _password.Text;
            s.Passphrase = _passphrase.Text;
            s.LatencyMs = (int)_latency.Value;
            s.ExpectedBitrateMbps = (int)_bitrate.Value;
            s.DisplayMode = (DisplayMode)Math.Max(0, _displayMode.SelectedIndex);
            s.ToneMapping = _toneMapping.SelectedItem as string ?? ReceiverSettings.DefaultToneMapping;
            s.TargetPeakNits = (int)_targetPeak.Value;
            s.HwDec = (HwDecMode)Math.Max(0, _hwdec.SelectedIndex);
            s.BufferSeconds = (double)_buffer.Value;
            s.Fullscreen = _fullscreen.Checked;
            s.FullscreenScreen = (int)_screen.Value;
            s.Frontend = (ReceiverFrontend)Math.Max(0, _frontend.SelectedIndex);
            s.MpvPath = _mpvPath.Text.Trim();
            s.FfmpegPath = _ffmpegPath.Text.Trim();
            s.AutoCatchUp = _catchUp.Checked;
            return s;
        }

        private static decimal Clamp(decimal value, NumericUpDown n) => Math.Min(Math.Max(value, n.Minimum), n.Maximum);

        private void SyncLatencyPreset()
        {
            int ms = (int)_latency.Value;
            int idx = ReceiverSettings.LatencyPresets.Length; // Custom
            for (int i = 0; i < ReceiverSettings.LatencyPresets.Length; i++)
            {
                if (ReceiverSettings.LatencyPresets[i].LatencyMs == ms) { idx = i; break; }
            }
            bool prev = _suppressEvents;
            _suppressEvents = true;
            _latencyPreset.SelectedIndex = idx;
            _suppressEvents = prev;
        }

        private void SaveSettings()
        {
            try
            {
                ReadUiIntoSettings().Save(_settingsPath);
            }
            catch (Exception ex)
            {
                AppendLog("[app] could not save settings: " + ex.Message);
            }
        }

        private void UpdateEnabledState()
        {
            bool running = _runner.IsRunning;
            var mode = (ConnectionMode)Math.Max(0, _mode.SelectedIndex);
            bool relay = mode == ConnectionMode.Relay;
            _host.Enabled = !running && mode != ConnectionMode.DirectListen;
            _path.Enabled = !running && relay;
            _user.Enabled = !running && relay;
            _password.Enabled = !running && relay;
            _toneMapping.Enabled = !running && _displayMode.SelectedIndex == (int)DisplayMode.SdrToneMap;
            bool pipe = _frontend.SelectedIndex == (int)ReceiverFrontend.FfmpegPipe;
            _ffmpegPath.Enabled = pipe;

            foreach (Control c in new Control[] { _mode, _port, _passphrase, _latencyPreset, _latency, _bitrate, _displayMode, _targetPeak, _hwdec, _buffer, _fullscreen, _screen, _frontend, _mpvPath })
                c.Enabled = !running && (c != _host);
            _host.Enabled = !running && mode != ConnectionMode.DirectListen;

            _play.Enabled = !running;
            _stop.Enabled = running;
            _fsToggle.Enabled = running;
        }

        private void RefreshToolLabels()
        {
            string? mpv = ToolLocator.Resolve(_mpvPath.Text, MpvExeName);
            _mpvFound.Text = mpv != null ? "found: " + mpv : "not found - download a shinchiro / zhongfly build (includes libsrt) and put mpv.exe next to this exe";
            _mpvFound.ForeColor = mpv != null ? SystemColors.GrayText : Color.Firebrick;
            string? ff = ToolLocator.Resolve(_ffmpegPath.Text, FfmpegExeName);
            _ffmpegFound.Text = ff != null ? "found: " + ff : "not found (only needed for the ffmpeg pipe frontend; gyan.dev full build has libsrt)";
            _ffmpegFound.ForeColor = ff != null ? SystemColors.GrayText : SystemColors.GrayText;
        }

        private void Browse(TextBox target, string exeName)
        {
            using (var dlg = new OpenFileDialog
            {
                Title = "Locate " + exeName,
                Filter = exeName + "|" + exeName + "|Executables (*.exe)|*.exe|All files (*.*)|*.*",
                CheckFileExists = true,
            })
            {
                if (dlg.ShowDialog(this) == DialogResult.OK) target.Text = dlg.FileName;
            }
        }

        // ---- actions ----------------------------------------------------------------------

        private MpvLaunchPlan? BuildPlan(bool interactive)
        {
            var s = ReadUiIntoSettings();
            var problems = s.Validate();
            if (problems.Count > 0)
            {
                if (interactive) MessageBox.Show(this, string.Join("\n", problems.ToArray()), "Check the settings", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return null;
            }
            string? mpv = ToolLocator.Resolve(s.MpvPath, MpvExeName);
            if (mpv == null)
            {
                if (interactive) MessageBox.Show(this, MpvDownloadHint, "mpv not found", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                mpv = MpvExeName; // still lets "Copy command" produce something useful
                if (interactive) return null;
            }
            string? ffmpeg = null;
            if (s.Frontend == ReceiverFrontend.FfmpegPipe)
            {
                ffmpeg = ToolLocator.Resolve(s.FfmpegPath, FfmpegExeName);
                if (ffmpeg == null)
                {
                    if (interactive)
                    {
                        MessageBox.Show(this, "ffmpeg.exe was not found. The ffmpeg pipe frontend needs ffmpeg (gyan.dev full build includes libsrt).",
                            "ffmpeg not found", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                        return null;
                    }
                    ffmpeg = FfmpegExeName;
                }
            }
            try
            {
                return MpvCommandBuilder.Build(s, Process.GetCurrentProcess().Id, mpv, ffmpeg);
            }
            catch (Exception ex)
            {
                if (interactive) MessageBox.Show(this, ex.Message, "Cannot build the command", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return null;
            }
        }

        private void Play()
        {
            if (_runner.IsRunning) return;
            var plan = BuildPlan(interactive: true);
            if (plan == null) return;
            SaveSettings();
            var plan2 = SrtBufferSizing.Compute(_settings.LatencyMs, (long)_settings.ExpectedBitrateMbps * 1_000_000L);
            AppendLog("[app] SRT buffer: " + plan2.Packets + " packets in flight -> ffs=" + plan2.FlowWindow + " rcvbuf=" + plan2.BufferBytes + " bytes");
            try
            {
                _runner.Start(_settings, plan);
            }
            catch (Exception ex)
            {
                MessageBox.Show(this, ex.Message, "Could not start", MessageBoxButtons.OK, MessageBoxIcon.Error);
            }
            UpdateEnabledState();
        }

        private void StopAsync()
        {
            if (!_runner.IsRunning) return;
            // Play has to go too: MpvRunner.Stop() flips IsRunning to false within microseconds but then spends
            // seconds killing mpv, and a Play in that window would start a second mpv on the same IPC pipe.
            _play.Enabled = false;
            _stop.Enabled = false;
            _fsToggle.Enabled = false;
            ThreadPool.QueueUserWorkItem(_ =>
            {
                try { _runner.Stop(); }
                catch (Exception ex) { ReceiverLog.Write("app", "stop failed: " + ex); }
                SafeInvoke(UpdateEnabledState);
            });
        }

        private void CopyCommand()
        {
            var plan = BuildPlan(interactive: false);
            if (plan == null)
            {
                MessageBox.Show(this, "The settings are incomplete; fix them first (see Play).", "Copy command", MessageBoxButtons.OK, MessageBoxIcon.Information);
                return;
            }
            SaveSettings();
            string cmd = plan.ToCommandLine();
            try
            {
                Clipboard.SetText(cmd);
                AppendLog("[app] command copied to clipboard (contains the passphrase - share with care)");
            }
            catch (Exception ex)
            {
                AppendLog("[app] clipboard failed: " + ex.Message);
            }
            AppendLog("[cmd] " + cmd);
        }

        private void OpenLogs()
        {
            string dir = AppPaths.LogsDir;
            AppPaths.EnsureDir(dir);
            try
            {
                Process.Start(new ProcessStartInfo { FileName = dir, UseShellExecute = true });
            }
            catch (Exception ex)
            {
                MessageBox.Show(this, "Could not open " + dir + "\n" + ex.Message, "Open logs", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            }
        }

        private void OnFormClosing(object? sender, FormClosingEventArgs e)
        {
            if (_closing) return;
            _closing = true;
            _uiTimer.Stop();
            SaveSettings();
            try
            {
                // Also runs on Windows session end (CloseReason.WindowsShutDown): never leave mpv behind.
                _runner.Stop();
                _runner.Dispose();
            }
            catch (Exception ex)
            {
                ReceiverLog.Write("app", "stop on close failed: " + ex);
            }
            // Closing the job kills anything still running, including after a crash on another thread.
            _job.Dispose();
        }

        // ---- runner callbacks (worker threads) --------------------------------------------

        private void OnRunnerLog(string line)
        {
            // mpv echoes the whole URL when a connection fails, passphrase included.
            line = SecretRedactor.Redact(line);
            lock (_pendingLog)
            {
                _pendingLog.Enqueue(line);
                while (_pendingLog.Count > MaxLogLines) _pendingLog.Dequeue();
            }
        }

        private void OnRunnerState(RunnerState state, string text)
        {
            SafeInvoke(() =>
            {
                _stState.Text = text;
                _stState.ForeColor = state == RunnerState.Playing ? Color.ForestGreen
                    : state == RunnerState.WaitingForStream ? Color.DarkOrange
                    : SystemColors.ControlText;
                UpdateEnabledState();
            });
        }

        private void SafeInvoke(Action action)
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

        private void AppendLog(string line)
        {
            // Everything that reaches the log file or the log box is masked here, whatever its source
            // ("Copy command" used to write the full URL, passphrase included, straight into the log file).
            line = SecretRedactor.Redact(line);
            ReceiverLog.Write(line);
            lock (_pendingLog) _pendingLog.Enqueue(line);
        }

        private void UiTick()
        {
            FlushLog();
            var st = _runner.Status;
            if (_runner.IsRunning)
            {
                var up = _runner.Uptime;
                _stUptime.Text = string.Format(CultureInfo.InvariantCulture, "{0:00}:{1:00}:{2:00}  (attempt {3})", (int)up.TotalHours, up.Minutes, up.Seconds, _runner.Attempt);
            }
            else
            {
                _stUptime.Text = "-";
            }
            _stCache.Text = st.DescribeCache();
            _stDrops.Text = st.FrameDrops + " (vo) / " + st.DecoderFrameDrops + " (decoder)";
            _stFps.Text = st.Fps.HasValue ? st.Fps.Value.ToString("0.0", CultureInfo.InvariantCulture) : "-";
            _stBitrate.Text = st.DescribeBitrate();
            _stHwdec.Text = string.IsNullOrEmpty(st.HwDec) ? "-" : st.HwDec;
            _stSource.Text = st.Source?.Describe() ?? "-";
            _stTarget.Text = st.Target?.Describe() ?? "-";
            if (st.Target == null) _stOutput.Text = "-";
            else if (st.Target.IsHdr) _stOutput.Text = "HDR passthrough active (" + st.Target.Gamma + ")";
            else if (st.Source != null && st.Source.IsHdr) _stOutput.Text = "SDR output: mpv is tone-mapping HDR source (Windows HDR off or SDR mode)";
            else _stOutput.Text = "SDR";
        }

        private void FlushLog()
        {
            List<string>? fresh = null;
            lock (_pendingLog)
            {
                if (_pendingLog.Count == 0) return;
                fresh = new List<string>(_pendingLog);
                _pendingLog.Clear();
            }
            _logLines.AddRange(fresh);
            if (_logLines.Count > MaxLogLines) _logLines.RemoveRange(0, _logLines.Count - MaxLogLines);
            _log.Text = string.Join(Environment.NewLine, _logLines.ToArray());
            _log.SelectionStart = _log.TextLength;
            _log.ScrollToCaret();
        }
    }
}
