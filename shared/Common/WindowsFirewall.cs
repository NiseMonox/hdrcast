using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

namespace HdrCast.Common
{
    /// <summary>One Windows Firewall rule, reduced to what decides whether a friend's UDP reaches HdrCast.</summary>
    public sealed class FirewallRule
    {
        public string Name { get; set; } = "";
        public string Application { get; set; } = "";
        public bool Enabled { get; set; }
        public bool Inbound { get; set; }
        public bool Allow { get; set; }
        /// <summary>6 TCP, 17 UDP, 256 any.</summary>
        public int Protocol { get; set; } = WindowsFirewall.ProtocolAny;
        /// <summary>NET_FW_PROFILE2 bits: 1 domain, 2 private, 4 public.</summary>
        public int Profiles { get; set; } = WindowsFirewall.ProfileAll;
        public string LocalPorts { get; set; } = "*";
        public string RemoteAddresses { get; set; } = "*";
    }

    public enum FirewallVerdict
    {
        /// <summary>The settings could not be read (not Windows, or another firewall product owns them).</summary>
        Unknown = 0,
        /// <summary>Windows Firewall is off for the networks in use.</summary>
        Off = 1,
        /// <summary>An allow rule lets UDP from any address and port reach the program.</summary>
        Allowed = 2,
        /// <summary>No such rule: only answers to what the program sent get in.</summary>
        NotAllowed = 3,
        /// <summary>A block rule for the program (e.g. from answering Cancel on Windows' own prompt) beats any allow rule.</summary>
        Blocked = 4,
        /// <summary>"Block all incoming connections" is on for a network in use: no rule can help.</summary>
        BlockAll = 5,
    }

    /// <summary>
    /// Whether Windows Firewall lets friends reach this program, and the one-time fix (docs/network.md §5). Without a
    /// rule Windows only lets in packets from an address and port the program sent to; a friend behind a router that
    /// picks a new port per destination arrives from a port we never sent to. The rule is per program path, so a new
    /// version in a new folder needs its own. Reading needs no rights; adding the rule runs this exe elevated.
    /// </summary>
    public static class WindowsFirewall
    {
        public const int ProfileAll = 0x7FFFFFFF;
        public const int ProtocolUdp = 17;
        public const int ProtocolAny = 256;
        /// <summary>The command-line verb both apps answer with "add the rule for me and exit" (run elevated).</summary>
        public const string AllowVerb = "firewall-allow";

        const int ErrorCancelled = 1223;          // the user said No on the UAC prompt

        /// <summary>Pure: what the rules mean for unsolicited UDP to <paramref name="exePath"/> on the active profiles.</summary>
        public static FirewallVerdict Evaluate(string exePath, IEnumerable<FirewallRule> rules, int activeProfiles,
                                               int enabledProfiles, int blockAllProfiles)
        {
            int guarded = activeProfiles & enabledProfiles;
            if (guarded == 0) return FirewallVerdict.Off;
            if ((guarded & blockAllProfiles) != 0) return FirewallVerdict.BlockAll;
            int covered = 0;
            bool blocked = false;
            foreach (FirewallRule r in rules)
            {
                if (!r.Enabled || !r.Inbound || (r.Protocol != ProtocolUdp && r.Protocol != ProtocolAny)) continue;
                if ((r.Profiles & guarded) == 0 || !SameProgram(r.Application, exePath)) continue;
                if (!r.Allow) blocked = true;
                else if (IsAny(r.LocalPorts) && IsAny(r.RemoteAddresses)) covered |= r.Profiles;   // our port is random
            }
            if (blocked) return FirewallVerdict.Blocked;
            return (covered & guarded) == guarded ? FirewallVerdict.Allowed : FirewallVerdict.NotAllowed;
        }

        /// <summary>The same file, however the rule spells it (case, environment variables, relative parts).</summary>
        public static bool SameProgram(string? ruleApplication, string exePath)
        {
            if (string.IsNullOrWhiteSpace(ruleApplication)) return false;     // rules for every program are not ours to judge
            return string.Equals(Normalize(ruleApplication!), Normalize(exePath), StringComparison.OrdinalIgnoreCase);
        }

        static string Normalize(string path)
        {
            string p = Environment.ExpandEnvironmentVariables(path.Trim().Trim('"'));
            try { p = Path.GetFullPath(p); } catch (Exception) { }
            return p.Replace('/', '\\').TrimEnd('\\');
        }

        static bool IsAny(string? value) => string.IsNullOrEmpty(value) || value == "*" || value!.Equals("Any", StringComparison.OrdinalIgnoreCase);

        static bool IsWindows => Environment.OSVersion.Platform == PlatformID.Win32NT;

        /// <summary>Reads the live settings; <see cref="FirewallVerdict.Unknown"/> when they cannot be read.</summary>
        public static FirewallVerdict Check(string exePath)
        {
            if (!IsWindows) return FirewallVerdict.Unknown;
            try
            {
                object policy = Create("HNetCfg.FwPolicy2");
                int active = Convert.ToInt32(Get(policy, "CurrentProfileTypes"));
                int enabled = 0, blockAll = 0;
                foreach (int p in new[] { 1, 2, 4 })
                {
                    if (Convert.ToBoolean(GetIndexed(policy, "FirewallEnabled", p))) enabled |= p;
                    if (Convert.ToBoolean(GetIndexed(policy, "BlockAllInboundTraffic", p))) blockAll |= p;
                }
                List<FirewallRule> mine = RulesFor(policy, exePath);
                Release(policy);
                return Evaluate(exePath, mine, active, enabled, blockAll);
            }
            catch (Exception)
            {
                return FirewallVerdict.Unknown;
            }
        }

        /// <summary>Every rule (inbound or outbound, any state) that names <paramref name="exePath"/>; null when unreadable.</summary>
        public static List<FirewallRule>? RulesFor(string exePath)
        {
            if (!IsWindows) return null;
            try
            {
                object policy = Create("HNetCfg.FwPolicy2");
                List<FirewallRule> rules = RulesFor(policy, exePath);
                Release(policy);
                return rules;
            }
            catch (Exception)
            {
                return null;
            }
        }

        static List<FirewallRule> RulesFor(object policy, string exePath)
        {
            var mine = new List<FirewallRule>();
            foreach (object rule in Enumerate(Get(policy, "Rules")))
            {
                // One property per rule first: a Windows 11 PC has hundreds of rules.
                string app = Get(rule, "ApplicationName") as string ?? "";
                if (SameProgram(app, exePath)) mine.Add(Read(rule, app));
                Release(rule);
            }
            return mine;
        }

        /// <summary>
        /// Needs administrator rights: switches off inbound block rules for <paramref name="exePath"/> and adds an allow
        /// rule (UDP, every network type) unless one already covers it. Null on success, else the error.
        /// </summary>
        public static string? AllowProgram(string exePath, string ruleName, string description)
        {
            if (!IsWindows) return "Windows Firewall only exists on Windows";
            try
            {
                object policy = Create("HNetCfg.FwPolicy2");
                object rules = Get(policy, "Rules") ?? throw new InvalidOperationException("the firewall has no rule list");
                bool covered = false;
                foreach (object rule in Enumerate(rules))
                {
                    string app = Get(rule, "ApplicationName") as string ?? "";
                    if (SameProgram(app, exePath))
                    {
                        FirewallRule r = Read(rule, app);
                        if (r.Enabled && r.Inbound && !r.Allow && (r.Protocol == ProtocolUdp || r.Protocol == ProtocolAny))
                            Set(rule, "Enabled", false);           // switched off rather than deleted: the user can see it
                        else if (r.Enabled && r.Inbound && r.Allow && (r.Protocol == ProtocolUdp || r.Protocol == ProtocolAny) &&
                                 (r.Profiles & ProfileAll) == ProfileAll && IsAny(r.LocalPorts) && IsAny(r.RemoteAddresses))
                            covered = true;
                    }
                    Release(rule);
                }
                if (!covered)
                {
                    object rule = Create("HNetCfg.FWRule");
                    Set(rule, "Name", ruleName);
                    Set(rule, "Description", description);
                    Set(rule, "ApplicationName", exePath);
                    Set(rule, "Protocol", ProtocolUdp);
                    Set(rule, "Direction", 1);                     // NET_FW_RULE_DIR_IN
                    Set(rule, "Action", 1);                        // NET_FW_ACTION_ALLOW
                    Set(rule, "Profiles", ProfileAll);
                    Set(rule, "Grouping", "HdrCast");
                    Set(rule, "Enabled", true);
                    rules.GetType().InvokeMember("Add", BindingFlags.InvokeMethod, null, rules, new[] { rule });
                    Release(rule);
                }
                Release(rules);
                Release(policy);
                return null;
            }
            catch (TargetInvocationException ex)
            {
                return (ex.InnerException ?? ex).Message;
            }
            catch (Exception ex)
            {
                return ex.Message;
            }
        }

        /// <summary>
        /// Runs "<paramref name="exePath"/> firewall-allow" as administrator (Windows shows its UAC prompt) and waits.
        /// Null when the rule is in place, else a short reason ("cancelled", …).
        /// </summary>
        public static string? AllowElevated(string exePath, int timeoutMs = 180_000)
        {
            var psi = new ProcessStartInfo(exePath, AllowVerb) { UseShellExecute = true, Verb = "runas", WindowStyle = ProcessWindowStyle.Hidden };
            try
            {
                using (Process? p = Process.Start(psi))
                {
                    if (p == null) return "it did not start";
                    if (!p.WaitForExit(timeoutMs)) return "it did not finish";
                    return p.ExitCode == 0 ? null : "adding the rule failed (see the log)";
                }
            }
            catch (Win32Exception ex) when (ex.NativeErrorCode == ErrorCancelled)
            {
                return "cancelled";
            }
            catch (Exception ex)
            {
                return ex.Message;
            }
        }

        // ---- late-bound COM (HNetCfg.FwPolicy2 / HNetCfg.FWRule), so nothing extra is referenced ----

        static object Create(string progId)
        {
            Type t = Type.GetTypeFromProgID(progId) ?? throw new InvalidOperationException(progId + " is not registered");
            return Activator.CreateInstance(t) ?? throw new InvalidOperationException(progId + " could not be created");
        }

        static object? Get(object o, string name) => o.GetType().InvokeMember(name, BindingFlags.GetProperty, null, o, null);

        static object? GetIndexed(object o, string name, object index) =>
            o.GetType().InvokeMember(name, BindingFlags.GetProperty, null, o, new[] { index });

        static void Set(object o, string name, object value) =>
            o.GetType().InvokeMember(name, BindingFlags.SetProperty, null, o, new[] { value });

        static FirewallRule Read(object rule, string app) => new FirewallRule
        {
            Name = Get(rule, "Name") as string ?? "",
            Application = app,
            Enabled = Convert.ToBoolean(Get(rule, "Enabled")),
            Inbound = Convert.ToInt32(Get(rule, "Direction")) == 1,
            Allow = Convert.ToInt32(Get(rule, "Action")) == 1,
            Protocol = Convert.ToInt32(Get(rule, "Protocol")),
            Profiles = Convert.ToInt32(Get(rule, "Profiles")),
            LocalPorts = Get(rule, "LocalPorts") as string ?? "*",
            RemoteAddresses = Get(rule, "RemoteAddresses") as string ?? "*",
        };

        /// <summary>
        /// A COM collection through its _NewEnum (DISPID -4). The runtime usually hands that back already wrapped as a
        /// managed IEnumerator (EnumeratorViewOfEnumVariant); a bare IEnumVARIANT is walked by hand. Anything else
        /// throws, so a check reads as Unknown instead of "no rules".
        /// </summary>
        static IEnumerable<object> Enumerate(object? collection)
        {
            if (collection == null) throw new InvalidOperationException("no collection");
            object? e = collection.GetType().InvokeMember("[DISPID=-4]", BindingFlags.GetProperty, null, collection, null);
            if (e is System.Collections.IEnumerator managed)
            {
                while (managed.MoveNext())
                    if (managed.Current != null) yield return managed.Current;
            }
            else if (e is IEnumVARIANT items)
            {
                var one = new object[1];
                while (items.Next(1, one, IntPtr.Zero) == 0)
                    if (one[0] != null) yield return one[0];
                Release(items);
            }
            else throw new InvalidOperationException("the firewall's rule list cannot be enumerated");
        }

        static void Release(object? o)
        {
#pragma warning disable CA1416 // only reached on Windows
            try { if (o != null && Marshal.IsComObject(o)) Marshal.ReleaseComObject(o); } catch (Exception) { }
#pragma warning restore CA1416
        }
    }
}
