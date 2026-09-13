using System;
using System.Runtime.InteropServices;

namespace HdrCast.Sender.Capture
{
    /// <summary>Win32 P/Invoke used by the capture engine (console attach, timers, ctrl handler).</summary>
    internal static class NativeMethods
    {
        public const int ATTACH_PARENT_PROCESS = -1;
        public const int STD_OUTPUT_HANDLE = -11;
        public const int STD_ERROR_HANDLE = -12;
        public const uint FILE_TYPE_UNKNOWN = 0;
        public static readonly IntPtr INVALID_HANDLE_VALUE = new IntPtr(-1);

        public const uint CREATE_WAITABLE_TIMER_HIGH_RESOLUTION = 0x00000002;
        public const uint TIMER_ALL_ACCESS = 0x1F0003;
        public const uint WAIT_OBJECT_0 = 0;
        public const uint INFINITE = 0xFFFFFFFF;

        public const uint CTRL_C_EVENT = 0;
        public const uint CTRL_BREAK_EVENT = 1;
        public const uint CTRL_CLOSE_EVENT = 2;
        public const uint CTRL_LOGOFF_EVENT = 5;
        public const uint CTRL_SHUTDOWN_EVENT = 6;

        public delegate bool ConsoleCtrlHandler(uint ctrlType);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool AttachConsole(int dwProcessId);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern IntPtr GetStdHandle(int nStdHandle);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern uint GetFileType(IntPtr hFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool SetConsoleCtrlHandler(ConsoleCtrlHandler? handler, [MarshalAs(UnmanagedType.Bool)] bool add);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        public static extern IntPtr CreateWaitableTimerExW(IntPtr lpTimerAttributes, string? lpTimerName, uint dwFlags, uint dwDesiredAccess);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool SetWaitableTimer(IntPtr hTimer, ref long pDueTime, int lPeriod, IntPtr pfnCompletionRoutine, IntPtr lpArgToCompletionRoutine, [MarshalAs(UnmanagedType.Bool)] bool fResume);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern uint WaitForSingleObject(IntPtr hHandle, uint dwMilliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool CloseHandle(IntPtr hObject);

        [DllImport("winmm.dll")]
        public static extern uint timeBeginPeriod(uint uPeriod);

        [DllImport("winmm.dll")]
        public static extern uint timeEndPeriod(uint uPeriod);

        /// <summary>
        /// Makes Console.Out / Console.Error usable for the console verbs of this WinExe:
        /// if the parent already redirected the standard handles (GUI → capture child) nothing is done;
        /// otherwise attach to the parent's console so 'HdrCastSender.exe list-outputs' prints in the terminal.
        /// Must run before the first use of the Console class.
        /// </summary>
        public static void EnsureConsole()
        {
            try
            {
                IntPtr h = GetStdHandle(STD_OUTPUT_HANDLE);
                if (h != IntPtr.Zero && h != INVALID_HANDLE_VALUE && GetFileType(h) != FILE_TYPE_UNKNOWN) return;
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
            catch
            {
                // no console available: Console.Out will write to a null stream
            }
        }
    }
}
