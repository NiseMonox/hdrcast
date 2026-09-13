using System;
using System.Collections.Generic;
using System.IO;

namespace HdrCast.Common
{
    /// <summary>Finds an external executable (mpv.exe / ffmpeg.exe): next to our exe, in tools\, then on PATH.</summary>
    public static class ToolLocator
    {
        public const string ToolsFolderName = "tools";

        /// <summary>Directory containing the running executable.</summary>
        public static string AppDirectory
        {
            get
            {
                string dir = AppDomain.CurrentDomain.BaseDirectory ?? "";
                return dir.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
            }
        }

        public static string? Find(string exeName) =>
            Find(exeName, AppDirectory, Environment.GetEnvironmentVariable("PATH"), File.Exists);

        /// <summary>Testable core: first candidate for which <paramref name="exists"/> returns true.</summary>
        public static string? Find(string exeName, string appDir, string? pathEnv, Func<string, bool> exists)
        {
            foreach (string candidate in Candidates(exeName, appDir, pathEnv))
            {
                try
                {
                    if (exists(candidate)) return candidate;
                }
                catch (Exception)
                {
                    // unreadable directory: skip
                }
            }
            return null;
        }

        /// <summary>Candidate paths in probe order.</summary>
        public static IEnumerable<string> Candidates(string exeName, string appDir, string? pathEnv)
        {
            if (string.IsNullOrEmpty(exeName)) throw new ArgumentException("exe name required", nameof(exeName));
            if (!string.IsNullOrEmpty(appDir))
            {
                yield return Path.Combine(appDir, exeName);
                yield return Path.Combine(appDir, ToolsFolderName, exeName);
            }
            if (!string.IsNullOrEmpty(pathEnv))
            {
                foreach (string raw in pathEnv!.Split(Path.PathSeparator))
                {
                    string dir = raw.Trim().Trim('"');
                    if (dir.Length == 0) continue;
                    string full;
                    try
                    {
                        full = Path.Combine(dir, exeName);
                    }
                    catch (ArgumentException)
                    {
                        continue; // invalid characters in a PATH entry
                    }
                    yield return full;
                }
            }
        }

        /// <summary>
        /// Uses the configured path when it points to an existing file, otherwise auto-locates
        /// <paramref name="exeName"/>. Returns null when nothing is found.
        /// </summary>
        public static string? Resolve(string? configuredPath, string exeName)
        {
            if (!string.IsNullOrWhiteSpace(configuredPath))
            {
                string p = configuredPath!.Trim().Trim('"');
                try
                {
                    if (File.Exists(p)) return Path.GetFullPath(p);
                }
                catch (Exception)
                {
                    // fall through to auto-detection
                }
            }
            return Find(exeName);
        }
    }
}
