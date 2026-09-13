using System;
using System.Globalization;

namespace HdrCast.Sender.Capture
{
    /// <summary>Timestamped log lines on stderr (design.md §2.1: stdout is reserved for the JSON stats).</summary>
    internal static class Log
    {
        static readonly object Gate = new object();

        public static void Info(string message) => Write('I', message);
        public static void Warn(string message) => Write('W', message);
        public static void Error(string message) => Write('E', message);

        static void Write(char level, string message)
        {
            try
            {
                string line = DateTime.Now.ToString("HH:mm:ss.fff", CultureInfo.InvariantCulture) + " [" + level + "] " + message;
                lock (Gate)
                {
                    Console.Error.WriteLine(line);
                }
            }
            catch
            {
                // stderr closed: nothing we can do
            }
        }

        /// <summary>Formats an HRESULT as 0x8xxxxxxx.</summary>
        public static string Hr(int hresult) => "0x" + hresult.ToString("X8", CultureInfo.InvariantCulture);
    }
}
