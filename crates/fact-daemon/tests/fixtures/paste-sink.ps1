# Raw VT input sink for the maximum-size bracketed-paste regression.
param([string]$ReportPath)
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
public static class PasteSink {
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int n);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetConsoleMode(IntPtr h, uint mode);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetConsoleCP(uint codepage);
    [DllImport("kernel32.dll")] static extern bool ReadFile(IntPtr h, byte[] data, uint size, out uint read, IntPtr overlapped);
    public static void Run(string path) {
        var input = GetStdHandle(-10);
        if (!SetConsoleCP(65001) || !SetConsoleMode(input, 0x0200))
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        for (int i = 0; i < 120; i++) Console.Write("HISTORY_" + i + "\r\n");
        Console.Write("\x1b[?2004hPASTE_READY");
        var stream = new MemoryStream();
        var data = new byte[65536];
        uint count;
        while (ReadFile(input, data, (uint)data.Length, out count, IntPtr.Zero) && count > 0) {
            stream.Write(data, 0, (int)count);
            var bytes = stream.GetBuffer();
            int n = (int)stream.Length;
            if (n >= 6 && System.Text.Encoding.ASCII.GetString(bytes, n - 6, 6) == "\x1b[201~") {
                File.WriteAllBytes(path, stream.ToArray());
                // Emit no output: paste itself must reset history.
                stream.SetLength(0);
            }
        }
    }
}
'@
[PasteSink]::Run($ReportPath)
