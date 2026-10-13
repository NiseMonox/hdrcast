using System;
using System.Collections.Generic;
using System.IO;

namespace HdrCast.Capture
{
    /// <summary>One row of a process snapshot.</summary>
    public readonly struct ProcessEntry
    {
        public ProcessEntry(int pid, int parentPid, string exeName)
        {
            Pid = pid;
            ParentPid = parentPid;
            ExeName = exeName ?? "";
        }

        public int Pid { get; }
        public int ParentPid { get; }
        public string ExeName { get; }
    }

    /// <summary>
    /// Which process tree to leave out of the desktop audio (design.md §2.4), e.g. the voice chat, so friends in the
    /// call do not hear themselves. Windows' process-loopback capture excludes exactly one process and everything it
    /// started, and apps like Discord play their audio from a helper that the main process started, so the target is
    /// the topmost process with the chosen name. Pure logic; unit-tested.
    /// </summary>
    public static class AppAudioTarget
    {
        public const int MaxNameLength = 100;

        /// <summary>
        /// What the user typed or picked, reduced to an executable file name ("discord" → "discord.exe",
        /// "C:\Apps\Discord.exe" → "Discord.exe"); "" when nothing usable is left.
        /// </summary>
        public static string NormalizeExeName(string? name)
        {
            string s = (name ?? "").Trim().Trim('"').Trim();
            if (s.Length == 0) return "";
            int slash = s.LastIndexOfAny(new[] { '\\', '/' });
            if (slash >= 0) s = s.Substring(slash + 1);
            if (s.Length == 0 || s.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0) return "";
            if (!s.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) s += ".exe";
            return s.Length > MaxNameLength ? "" : s;
        }

        /// <summary>
        /// The root of the largest tree of processes named <paramref name="exeName"/> (a process whose parent has another
        /// name, or is gone), or 0 when none runs. Case-insensitive; a reused parent id with another name ends the walk.
        /// </summary>
        public static int FindRoot(IEnumerable<ProcessEntry> processes, string exeName)
        {
            if (string.IsNullOrEmpty(exeName)) return 0;
            var byPid = new Dictionary<int, ProcessEntry>();
            foreach (ProcessEntry p in processes) byPid[p.Pid] = p;

            var members = new Dictionary<int, int>();
            foreach (ProcessEntry p in byPid.Values)
            {
                if (!Named(p, exeName)) continue;
                ProcessEntry top = p;
                for (int depth = 0; depth < 64; depth++)        // bounded: parent ids can form a loop after reuse
                {
                    if (top.ParentPid == top.Pid || !byPid.TryGetValue(top.ParentPid, out ProcessEntry parent) || !Named(parent, exeName)) break;
                    top = parent;
                }
                members[top.Pid] = members.TryGetValue(top.Pid, out int n) ? n + 1 : 1;
            }

            int best = 0, bestCount = 0;
            foreach (KeyValuePair<int, int> kv in members)
            {
                if (kv.Value > bestCount || (kv.Value == bestCount && kv.Key < best))
                {
                    best = kv.Key;
                    bestCount = kv.Value;
                }
            }
            return best;
        }

        static bool Named(ProcessEntry p, string exeName) => string.Equals(p.ExeName, exeName, StringComparison.OrdinalIgnoreCase);
    }
}
