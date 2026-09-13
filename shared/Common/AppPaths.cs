using System;
using System.IO;

namespace HdrCast.Common
{
    /// <summary>%APPDATA%\HdrCast (settings) and %APPDATA%\HdrCast\logs (logs). On Linux test runs
    /// ApplicationData resolves to ~/.config.</summary>
    public static class AppPaths
    {
        public const string AppFolderName = "HdrCast";
        public const string LogsFolderName = "logs";

        public static string AppDataRoot =>
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData, Environment.SpecialFolderOption.DoNotVerify);

        public static string ConfigDir => ConfigDirUnder(AppDataRoot);
        public static string LogsDir => LogsDirUnder(AppDataRoot);

        public static string ConfigDirUnder(string appDataRoot) => Path.Combine(appDataRoot, AppFolderName);
        public static string LogsDirUnder(string appDataRoot) => Path.Combine(ConfigDirUnder(appDataRoot), LogsFolderName);

        public static string SettingsFile(string fileName) => Path.Combine(ConfigDir, fileName);

        /// <summary>logs\&lt;prefix&gt;-yyyyMMdd.log</summary>
        public static string DailyLogFile(string prefix, DateTime day) =>
            Path.Combine(LogsDir, prefix + "-" + day.ToString("yyyyMMdd", System.Globalization.CultureInfo.InvariantCulture) + ".log");

        /// <summary>Creates the directory when missing; never throws (returns false on failure).</summary>
        public static bool EnsureDir(string dir)
        {
            try
            {
                Directory.CreateDirectory(dir);
                return true;
            }
            catch (Exception)
            {
                return false;
            }
        }
    }
}
