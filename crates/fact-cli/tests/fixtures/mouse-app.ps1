# A deterministic VT application. No model, network or user data is used.
param([switch]$MainScreen, [string]$ReportPath = "")
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class MouseApp {
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int n);
    [DllImport("kernel32.dll")] static extern bool SetConsoleMode(IntPtr h, uint mode);
    [DllImport("kernel32.dll")] static extern bool ReadFile(IntPtr h, byte[] data, uint size, out uint read, IntPtr overlapped);
    public static void Run(bool mainScreen, string reportPath) {
        if (reportPath.Length > 0) System.IO.File.WriteAllText(reportPath, "");
        var input = GetStdHandle(-10);
        SetConsoleMode(input, 0x0200);
        if (mainScreen) {
            for (int line = 0; line < 120; line++) Console.Write("HISTORY_" + line.ToString("D3") + "\r\n");
            Console.Write("\x1b[?1003h\x1b[?1006h");
        } else {
            Console.Write("\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[2J\x1b[HFACT_MOUSE_APP");
        }
        var pending = "";
        var data = new byte[256];
        uint count;
        int tick = 0;
        var timer = new System.Threading.Timer(_ => Console.Write("\x1b[3;1HTICK_" + (++tick)), null, mainScreen ? -1 : 0, 100);
        while (ReadFile(input, data, (uint)data.Length, out count, IntPtr.Zero) && count > 0) {
            pending += System.Text.Encoding.ASCII.GetString(data, 0, (int)count);
            int end;
            while ((end = pending.IndexOfAny(new char[] {'M', 'm'})) >= 0) {
                var report = pending.Substring(0, end + 1);
                pending = pending.Substring(end + 1);
                if (reportPath.Length > 0) System.IO.File.AppendAllText(reportPath, report);
                Console.Write("\x1b[2;1H" + report.Replace("\x1b", "ESC") + "                ");
            }
        }
        GC.KeepAlive(timer);
    }
}
'@
[MouseApp]::Run($MainScreen.IsPresent, $ReportPath)
