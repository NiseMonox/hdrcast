using System;
using System.IO;
using Xunit;

namespace HdrCast.Tests
{
    /// <summary>
    /// Guards the shape of the distribution. A .cmd launcher whose PowerShell payload was not packaged looks
    /// fine in the repo and fails on the user's machine, which is exactly what happened to get-tools.cmd.
    /// </summary>
    public class PackagingTests
    {
        static string RepoRoot()
        {
            var dir = new DirectoryInfo(AppContext.BaseDirectory);
            while (dir != null && !File.Exists(Path.Combine(dir.FullName, "build.sh"))) dir = dir.Parent;
            return dir?.FullName ?? "";
        }

        [Fact]
        public void Every_script_a_cmd_launcher_calls_is_next_to_it()
        {
            string root = RepoRoot();
            if (root.Length == 0) return;
            string scripts = Path.Combine(root, "scripts");
            if (!Directory.Exists(scripts)) return;

            foreach (string cmd in Directory.GetFiles(scripts, "*.cmd"))
            {
                string text = File.ReadAllText(cmd);
                foreach (string line in text.Split('\n'))
                {
                    int at = line.IndexOf("%~dp0", StringComparison.OrdinalIgnoreCase);
                    if (at < 0) continue;
                    string rest = line.Substring(at + 5);
                    int end = rest.IndexOfAny(new[] { '"', ' ', '\r' });
                    string referenced = (end > 0 ? rest.Substring(0, end) : rest).Trim();
                    // Only sibling script files: "%~dp0.." is a path to the parent folder, not a reference.
                    if (referenced.Length == 0 || referenced.StartsWith("..", StringComparison.Ordinal)) continue;
                    string ext = Path.GetExtension(referenced).ToLowerInvariant();
                    if (ext != ".ps1" && ext != ".cmd" && ext != ".bat") continue;
                    Assert.True(File.Exists(Path.Combine(scripts, referenced)),
                        Path.GetFileName(cmd) + " calls " + referenced + ", which is not in scripts/");
                }
            }
        }

        [Fact]
        public void Build_script_packages_both_cmd_and_ps1()
        {
            string root = RepoRoot();
            if (root.Length == 0) return;
            string build = File.ReadAllText(Path.Combine(root, "build.sh"));
            Assert.Contains("scripts/*.cmd", build);
            Assert.Contains("scripts/*.ps1", build);
        }
    }
}
