using System;
using System.Globalization;
using System.IO;
using System.Text;
using HdrCast.Common;

namespace HdrCast.Sender
{
    /// <summary>Appends timestamped lines to %APPDATA%\HdrCast\logs\sender-yyyyMMdd.log. Never throws.</summary>
    internal static class SenderLog
    {
        private static readonly object Gate = new object();

        public static string CurrentFile => AppPaths.DailyLogFile("sender", DateTime.Now);

        public static void Write(string line)
        {
            try
            {
                lock (Gate)
                {
                    AppPaths.EnsureDir(AppPaths.LogsDir);
                    string stamp = DateTime.Now.ToString("HH:mm:ss.fff", CultureInfo.InvariantCulture);
                    File.AppendAllText(CurrentFile, stamp + " " + line + Environment.NewLine, new UTF8Encoding(false));
                }
            }
            catch (Exception)
            {
                // logging must never take the app down
            }
        }

        public static void Write(string prefix, string line) => Write("[" + prefix + "] " + line);
    }
}
