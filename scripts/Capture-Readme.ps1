#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ExecutablePath = '',
    [string]$OutputDirectory = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Native documentation capture requires Windows.' }
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ExecutablePath)) { $ExecutablePath = Join-Path $root 'target\debug\twilight-sync.exe' }
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Join-Path $root 'docs\images' }
$executable = Get-Item -LiteralPath $ExecutablePath
$null = [IO.Directory]::CreateDirectory($OutputDirectory)
Add-Type -AssemblyName System.Drawing
$typeName = 'TwilightReadmeCapture_' + [guid]::NewGuid().ToString('N')
$captureTypes = Add-Type -PassThru -TypeDefinition @"
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;

public static class $typeName {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr context, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr window, StringBuilder text, int length);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr window, IntPtr after, int left, int top, int width, int height, uint flags);
    [DllImport("user32.dll")] public static extern bool RedrawWindow(IntPtr window, IntPtr rect, IntPtr region, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("dwmapi.dll")] public static extern int DwmFlush();
    private delegate void WindowEvent(IntPtr hook, uint kind, IntPtr window, int objectId, int child, uint thread, uint time);
    private delegate bool EnumerateWindow(IntPtr window, IntPtr state);
    [StructLayout(LayoutKind.Sequential)]
    private struct Message { public IntPtr Window; public uint Id; public UIntPtr WParam; public IntPtr LParam; public uint Time; public int X, Y; public uint Private; }
    [DllImport("user32.dll")] private static extern IntPtr SetWinEventHook(uint first, uint last, IntPtr module, WindowEvent callback, uint process, uint thread, uint flags);
    [DllImport("user32.dll")] private static extern bool UnhookWinEvent(IntPtr hook);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumerateWindow callback, IntPtr state);
    [DllImport("user32.dll")] private static extern uint MsgWaitForMultipleObjectsEx(uint count, IntPtr handles, uint timeout, uint mask, uint flags);
    [DllImport("user32.dll")] private static extern bool PeekMessage(out Message message, IntPtr window, uint first, uint last, uint flags);
    [DllImport("user32.dll")] private static extern bool TranslateMessage(ref Message message);
    [DllImport("user32.dll")] private static extern IntPtr DispatchMessage(ref Message message);

    public static IntPtr WaitForPreview(uint process, int timeout) {
        IntPtr found = IntPtr.Zero;
        Action<IntPtr> inspect = window => {
            uint owner;
            GetWindowThreadProcessId(window, out owner);
            if (owner != process) return;
            var title = new StringBuilder(256);
            GetWindowText(window, title, title.Capacity);
            if (title.ToString() == "Twilight Sync - UI smoke") found = window;
        };
        WindowEvent callback = (hook, kind, window, objectId, child, thread, time) => inspect(window);
        IntPtr subscription = SetWinEventHook(0x8002, 0x800C, IntPtr.Zero, callback, process, 0, 0);
        if (subscription == IntPtr.Zero) throw new InvalidOperationException("Cannot observe the isolated preview window.");
        try {
            EnumWindows((window, state) => { inspect(window); return found == IntPtr.Zero; }, IntPtr.Zero);
            var clock = Stopwatch.StartNew();
            while (found == IntPtr.Zero && clock.ElapsedMilliseconds < timeout) {
                uint remaining = (uint)Math.Max(1, timeout - clock.ElapsedMilliseconds);
                MsgWaitForMultipleObjectsEx(0, IntPtr.Zero, remaining, 0x04FF, 4);
                Message message;
                while (PeekMessage(out message, IntPtr.Zero, 0, 0, 1)) {
                    TranslateMessage(ref message);
                    DispatchMessage(ref message);
                }
            }
            return found;
        } finally { UnhookWinEvent(subscription); GC.KeepAlive(callback); }
    }
}
"@
$capture = $captureTypes | Where-Object { $_.Name -ceq $typeName }

$views = @(
    @{ Name = 'dashboard.png'; Page = 'overview'; Theme = 'light'; Scenario = 'ready'; Width = 1380; Height = 1150; Scale = 1.0 },
    @{ Name = 'save-comparison.png'; Page = 'overview'; Theme = 'light'; Scenario = 'attention'; Width = 1480; Height = 1070; Scale = 0.85 },
    @{ Name = 'save-history.png'; Page = 'history'; Theme = 'dark'; Scenario = 'ready'; Width = 1380; Height = 1500; Scale = 0.8 }
)
foreach ($view in $views) {
    $start = [Diagnostics.ProcessStartInfo]::new($executable.FullName)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in @('--ui-smoke', '--page', $view.Page, '--theme', $view.Theme, '--scenario', $view.Scenario, '--scale', ([double]$view.Scale).ToString([Globalization.CultureInfo]::InvariantCulture))) {
        $start.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::Start($start)
    $previousDpiContext = [IntPtr]::Zero
    try {
        $window = $capture::WaitForPreview($process.Id, 15000)
        if ($window -eq [IntPtr]::Zero) { throw 'The isolated preview has no app window.' }
        [uint32]$owner = 0
        $null = $capture::GetWindowThreadProcessId($window, [ref]$owner)
        $title = [Text.StringBuilder]::new(256)
        $null = $capture::GetWindowText($window, $title, $title.Capacity)
        if ($owner -ne $process.Id -or $title.ToString() -cne 'Twilight Sync - UI smoke') {
            throw 'Refusing to capture anything other than the preview started by this script.'
        }
        $previousDpiContext = $capture::SetThreadDpiAwarenessContext([IntPtr](-4))
        if ($previousDpiContext -eq [IntPtr]::Zero) { throw 'Cannot use physical pixels for preview capture.' }
        $null = $capture::SetWindowPos($window, [IntPtr]::Zero, 20, 20, $view.Width, $view.Height, 0x0040)
        $null = $capture::RedrawWindow($window, [IntPtr]::Zero, [IntPtr]::Zero, 0x0185)
        $null = $capture::DwmFlush()
        $rect = [Activator]::CreateInstance($capture.GetNestedType('Rect'))
        if (-not $capture::GetClientRect($window, [ref]$rect)) { throw 'Cannot read preview dimensions.' }
        $bitmap = [Drawing.Bitmap]::new($rect.Right, $rect.Bottom)
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $context = $graphics.GetHdc()
            try {
                if (-not $capture::PrintWindow($window, $context, 3)) { throw 'Native preview capture failed.' }
            } finally { $graphics.ReleaseHdc($context) }
            $colors = [Collections.Generic.HashSet[int]]::new()
            for ($row = 0; $row -lt $bitmap.Height; $row += 8) {
                for ($column = 0; $column -lt $bitmap.Width; $column += 8) {
                    $null = $colors.Add($bitmap.GetPixel($column, $row).ToArgb())
                }
            }
            if ($colors.Count -lt 20) { throw 'The preview capture is blank; no screenshot was saved.' }
            $destination = Join-Path $OutputDirectory $view.Name
            $bitmap.Save($destination, [Drawing.Imaging.ImageFormat]::Png)
            Write-Host "Captured $($view.Name): $($bitmap.Width)x$($bitmap.Height), sample data only."
        } finally {
            $graphics.Dispose()
            $bitmap.Dispose()
        }
        if (-not $process.WaitForExit(30000)) { throw 'The isolated preview did not stop automatically.' }
        if ($process.ExitCode -ne 0) { throw $process.StandardError.ReadToEnd() }
    } finally {
        if ($previousDpiContext -ne [IntPtr]::Zero) { $null = $capture::SetThreadDpiAwarenessContext($previousDpiContext) }
        if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
}