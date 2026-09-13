using System;
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace HdrCast.Common
{
    /// <summary>
    /// A Windows Job Object with KILL_ON_JOB_CLOSE: every child assigned to it dies when this process does,
    /// including a hard kill from Task Manager or a crash (design.md §2.7). Without it, ffmpeg / the capture
    /// engine / mpv can be left running with no window and no way to stop them except Task Manager.
    ///
    /// Everything is best effort: on a platform or a sandbox where jobs are unavailable the calls fail
    /// silently and the app keeps working (the normal Stop path still kills the process tree).
    /// </summary>
    public sealed class ProcessJob : IDisposable
    {
        const int JobObjectExtendedLimitInformation = 9;
        const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000;

        [StructLayout(LayoutKind.Sequential)]
        struct IO_COUNTERS
        {
            public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
            public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        struct JOBOBJECT_BASIC_LIMIT_INFORMATION
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr CreateJobObjectW(IntPtr lpJobAttributes, string? lpName);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        static extern bool SetInformationJobObject(IntPtr hJob, int JobObjectInformationClass,
                                                   IntPtr lpJobObjectInformation, uint cbJobObjectInformationLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        static extern bool CloseHandle(IntPtr hObject);

        IntPtr _handle;

        /// <summary>True when the job exists; false means children are not tied to this process's lifetime.</summary>
        public bool IsAvailable => _handle != IntPtr.Zero;

        public ProcessJob()
        {
            try
            {
                _handle = CreateJobObjectW(IntPtr.Zero, null);
                if (_handle == IntPtr.Zero) return;

                var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                int size = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
                IntPtr buffer = Marshal.AllocHGlobal(size);
                try
                {
                    Marshal.StructureToPtr(info, buffer, false);
                    if (!SetInformationJobObject(_handle, JobObjectExtendedLimitInformation, buffer, (uint)size))
                    {
                        CloseHandle(_handle);
                        _handle = IntPtr.Zero;
                    }
                }
                finally { Marshal.FreeHGlobal(buffer); }
            }
            catch (Exception)
            {
                _handle = IntPtr.Zero;   // not Windows, or jobs unavailable
            }
        }

        /// <summary>Ties a started child process to this job. Returns false when it could not be assigned.</summary>
        public bool Assign(Process process)
        {
            if (_handle == IntPtr.Zero || process == null) return false;
            try
            {
                if (process.HasExited) return false;
                return AssignProcessToJobObject(_handle, process.Handle);
            }
            catch (Exception)
            {
                // already exited, or the process is in another job that disallows nesting
                return false;
            }
        }

        public void Dispose()
        {
            if (_handle == IntPtr.Zero) return;
            try { CloseHandle(_handle); }
            catch (Exception) { }
            _handle = IntPtr.Zero;
        }
    }
}
