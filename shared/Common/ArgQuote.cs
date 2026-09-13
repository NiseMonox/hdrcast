using System.Collections.Generic;
using System.Text;

namespace HdrCast.Common
{
    /// <summary>
    /// Quotes command-line arguments the way Windows CreateProcess / the MSVCRT argv parser expects
    /// (the same rules .NET uses to split <c>ProcessStartInfo.Arguments</c> on every platform, so the
    /// output is safe for <see cref="LineProcess"/> on Windows and on Linux test runs alike).
    /// Rules: an argument without whitespace or quotes is passed verbatim; otherwise it is wrapped in
    /// double quotes, embedded quotes become \" and backslashes immediately before a quote (or at the
    /// end of the argument) are doubled. Backslashes elsewhere are left untouched.
    /// </summary>
    public static class ArgQuote
    {
        public static string Quote(string? arg)
        {
            arg ??= "";
            if (arg.Length > 0 && !NeedsQuoting(arg)) return arg;

            var sb = new StringBuilder(arg.Length + 2);
            sb.Append('"');
            int i = 0;
            while (i < arg.Length)
            {
                int backslashes = 0;
                while (i < arg.Length && arg[i] == '\\')
                {
                    backslashes++;
                    i++;
                }

                if (i == arg.Length)
                {
                    // Trailing backslashes must be doubled so they do not escape the closing quote.
                    sb.Append('\\', backslashes * 2);
                }
                else if (arg[i] == '"')
                {
                    sb.Append('\\', backslashes * 2 + 1);
                    sb.Append('"');
                    i++;
                }
                else
                {
                    sb.Append('\\', backslashes);
                    sb.Append(arg[i]);
                    i++;
                }
            }
            sb.Append('"');
            return sb.ToString();
        }

        public static string Join(IEnumerable<string> args)
        {
            var sb = new StringBuilder();
            foreach (var a in args)
            {
                if (sb.Length > 0) sb.Append(' ');
                sb.Append(Quote(a));
            }
            return sb.ToString();
        }

        /// <summary>Full command line: quoted executable followed by the quoted arguments.</summary>
        public static string CommandLine(string exe, IEnumerable<string> args)
        {
            string joined = Join(args);
            return joined.Length == 0 ? Quote(exe) : Quote(exe) + " " + joined;
        }

        private static bool NeedsQuoting(string s)
        {
            foreach (char c in s)
            {
                if (c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '"') return true;
                // Not needed by CreateProcess, but these characters break a command the user copies into
                // cmd.exe — and an SRT URL is full of '&'. Quoting them is harmless for CreateProcess.
                if (c == '&' || c == '|' || c == '<' || c == '>' || c == '^') return true;
            }
            return false;
        }
    }
}
