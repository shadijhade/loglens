# Drives a running LogLens window for UI checks without touching the real mouse or keyboard,
# by posting window messages. Coordinates are client pixels, as they appear in a screenshot.
#
#   uitest.ps1 shot  <out.png>
#   uitest.ps1 click <x> <y>
#   uitest.ps1 drag  <x1> <y1> <x2> <y2>
#   uitest.ps1 move  <x> <y>
#   uitest.ps1 text  <string>
#   uitest.ps1 key   <virtual-key code>
#   uitest.ps1 wheel <x> <y> <delta>
#
# Hover effects need the real cursor over the window: Windows answers a synthetic move with an
# immediate WM_MOUSELEAVE when the pointer is elsewhere.
param([string]$cmd, [Parameter(ValueFromRemainingArguments = $true)][string[]]$a)

Add-Type -ReferencedAssemblies System.Drawing @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public static class W {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll", EntryPoint="PostMessageW")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    public static void Shot(IntPtr h, string path) {
        RECT wr; GetWindowRect(h, out wr);
        RECT cr; GetClientRect(h, out cr);
        POINT o = new POINT(); ClientToScreen(h, ref o);
        using (var full = new Bitmap(wr.R - wr.L, wr.B - wr.T))
        {
            using (var g = Graphics.FromImage(full)) { IntPtr dc = g.GetHdc(); PrintWindow(h, dc, 2); g.ReleaseHdc(dc); }
            // Crop to the client area so screenshot pixels equal client coordinates.
            var crop = new Rectangle(o.X - wr.L, o.Y - wr.T, cr.R, cr.B);
            using (var client = full.Clone(crop, full.PixelFormat)) client.Save(path, System.Drawing.Imaging.ImageFormat.Png);
        }
    }
    public static IntPtr L(int x, int y) { return (IntPtr)((y << 16) | (x & 0xFFFF)); }
}
'@
[void][W]::SetProcessDPIAware()
$p = Get-Process loglens -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { "loglens window not found"; exit 1 }
$h = $p.MainWindowHandle
$WM_MOUSEMOVE = 0x200; $WM_LDOWN = 0x201; $WM_LUP = 0x202; $WM_KEYDOWN = 0x100; $WM_KEYUP = 0x101; $WM_CHAR = 0x102; $WM_WHEEL = 0x20A

switch ($cmd) {
    'shot' { [W]::Shot($h, $a[0]); "saved $($a[0])" }
    'move' { [void][W]::PostMessage($h, $WM_MOUSEMOVE, [IntPtr]0, [W]::L($a[0], $a[1])) }
    'click' {
        $l = [W]::L($a[0], $a[1])
        [void][W]::PostMessage($h, $WM_MOUSEMOVE, [IntPtr]0, $l); Start-Sleep -Milliseconds 60
        [void][W]::PostMessage($h, $WM_LDOWN, [IntPtr]1, $l); Start-Sleep -Milliseconds 60
        [void][W]::PostMessage($h, $WM_LUP, [IntPtr]0, $l)
    }
    'drag' {
        $x1 = [int]$a[0]; $y1 = [int]$a[1]; $x2 = [int]$a[2]; $y2 = [int]$a[3]
        [void][W]::PostMessage($h, $WM_MOUSEMOVE, [IntPtr]0, [W]::L($x1, $y1)); Start-Sleep -Milliseconds 60
        [void][W]::PostMessage($h, $WM_LDOWN, [IntPtr]1, [W]::L($x1, $y1)); Start-Sleep -Milliseconds 60
        1..8 | ForEach-Object {
            $x = $x1 + ($x2 - $x1) * $_ / 8; $y = $y1 + ($y2 - $y1) * $_ / 8
            [void][W]::PostMessage($h, $WM_MOUSEMOVE, [IntPtr]1, [W]::L([int]$x, [int]$y)); Start-Sleep -Milliseconds 40
        }
        [void][W]::PostMessage($h, $WM_LUP, [IntPtr]0, [W]::L($x2, $y2))
    }
    'text' { foreach ($c in $a[0].ToCharArray()) { [void][W]::PostMessage($h, $WM_CHAR, [IntPtr][int]$c, [IntPtr]1); Start-Sleep -Milliseconds 25 } }
    'key' {
        [void][W]::PostMessage($h, $WM_KEYDOWN, [IntPtr][int]$a[0], [IntPtr]1); Start-Sleep -Milliseconds 40
        [void][W]::PostMessage($h, $WM_KEYUP, [IntPtr][int]$a[0], [IntPtr]0xC0000001)
    }
    'wheel' {
        # Wheel coordinates are screen-relative.
        $pt = New-Object W+POINT; $pt.X = [int]$a[0]; $pt.Y = [int]$a[1]; [void][W]::ClientToScreen($h, [ref]$pt)
        $w = [IntPtr]([int]$a[2] -shl 16)
        [void][W]::PostMessage($h, $WM_WHEEL, $w, [W]::L($pt.X, $pt.Y))
    }
}
