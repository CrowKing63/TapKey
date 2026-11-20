using System;
using System.Drawing;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;

namespace TapKey.PoC;

internal static class Program
{
    const int WindowMs = 300;
    const int RadiusPx = 25;
    const int SameTapGapMs = 80;
    const float DpiScale = 1.5f; // physical→logical divisor (example: 150% display scaling)

    static string _logPath;
    static DateTime _start = DateTime.Now;
    static IntPtr _hookId;
    static LowLevelMouseProc _hookProc;

    static int _lastTapX, _lastTapY;
    static long _lastTapTimeMs;
    static bool _hasLastTap;
    static long _curGroupStartMs;
    static bool _inGroup;

    static OverlayForm? _overlay;

    [STAThread]
    static void Main(string[] args)
    {
        var logDir = Path.Combine(AppContext.BaseDirectory, "logs");
        Directory.CreateDirectory(logDir);
        _logPath = Path.Combine(logDir, $"tapkey-poc-{DateTime.Now:yyyyMMdd-HHmmss}.log");
        File.WriteAllText(_logPath, $"# TapKey PoC {DateTime.Now:O}\n");

        _hookProc = (LowLevelMouseProc)HookCallback;
        _hookId = SetWindowsHookEx(14, _hookProc, IntPtr.Zero, 0);
        int err = Marshal.GetLastWin32Error();
        Log($"HOOK: 0x{_hookId.ToInt64():X} err={err}");

        var f = new Form();
        f.ShowInTaskbar = false;
        f.FormBorderStyle = FormBorderStyle.None;
        f.Size = new Size(1, 1);
        f.Location = new Point(-2000, -2000);
        f.Opacity = 0.01f;
        f.FormClosed += (s, e) => { try { UnhookWindowsHookEx(_hookId); } catch { } Application.Exit(); };

        var timer = new System.Threading.Timer(_ => Application.Exit(), null, TimeSpan.FromMinutes(5), TimeSpan.FromMinutes(5));
        Log("READY — double-tap to open overlay.");
        Application.Run(f);
    }

    delegate IntPtr LowLevelMouseProc(int nCode, IntPtr wParam, IntPtr lParam);

    static IntPtr HookCallback(int nCode, IntPtr wParam, IntPtr lParam)
    {
        if (nCode != 0) return CallNextHookEx(_hookId, nCode, wParam, lParam);

        unsafe
        {
            byte* p = (byte*)lParam;
            int cx = *(int*)p;           // physical x
            int cy = *(int*)(p + 4);     // physical y

            long now = (long)(DateTime.Now - _start).TotalMilliseconds;

            bool isNewTap = !_inGroup || (now - _curGroupStartMs) > SameTapGapMs;

            if (isNewTap)
            {
                if (_hasLastTap && (now - _lastTapTimeMs) <= WindowMs
                    && Math.Abs(cx - _lastTapX) <= RadiusPx
                    && Math.Abs(cy - _lastTapY) <= RadiusPx)
                {
                    Log($"DOUBLE-TAP phys=({cx},{cy})");
                    _hasLastTap = false;
                    ShowOverlay(cx, cy);
                }

                _lastTapX = cx;
                _lastTapY = cy;
                _lastTapTimeMs = now;
                _hasLastTap = true;
            }

            if (isNewTap) _curGroupStartMs = now;
            _inGroup = true;
        }

        return CallNextHookEx(_hookId, nCode, wParam, lParam);
    }

    static void ShowOverlay(int physX, int physY)
    {
        // Convert to logical for both WinForms AND SetCursorPos (DPI-unaware app)
        int logX = (int)(physX / DpiScale);
        int logY = (int)(physY / DpiScale);

        if (_overlay != null && _overlay.IsHandleCreated)
        {
            _overlay.Close();
            _overlay = null;
        }
        _overlay = new OverlayForm(logX, logY);
        _overlay.Show();
    }

    static void Log(string text)
    {
        var line = $"[+{(DateTime.Now - _start).TotalSeconds,8:F2}s] {text}\n";
        Console.Write(line);
        try { File.AppendAllText(_logPath, line); } catch { }
    }

    // ── P/Invoke ──

    [DllImport("user32.dll")]
    static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll", SetLastError = true)]
    static extern IntPtr SetWindowsHookEx(int idHook, LowLevelMouseProc lpfn, IntPtr hMod, uint dwThreadId);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    static extern bool UnhookWindowsHookEx(IntPtr hhk);

    [DllImport("user32.dll")]
    static extern IntPtr CallNextHookEx(IntPtr hhk, int nCode, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll")]
    static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);

    // ── Win32 structs ──

    [StructLayout(LayoutKind.Sequential)]
    struct INPUT
    {
        public uint type;
        public uint mi_reserved;
        public MOUSEINPUT mi;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT
    {
        public int dx;
        public int dy;
        public uint mouseData;
        public uint dwFlags;
        public uint time;
        public IntPtr dwExtraInfo;
    }

    // ── Overlay Form ──

    class OverlayForm : Form
    {
        readonly int _x, _y; // LOGICAL coordinates (for both positioning and SetCursorPos)

        public OverlayForm(int x, int y)
        {
            _x = x;
            _y = y;

            FormBorderStyle = FormBorderStyle.FixedToolWindow;
            StartPosition = FormStartPosition.Manual;
            ShowInTaskbar = false;
            TopMost = true;
            BackColor = Color.FromArgb(30, 30, 40);
            ForeColor = Color.White;
            Font = new Font("Segoe UI", 9f);
            AutoScaleMode = AutoScaleMode.None;

            var btnRightClick = new Button { Text = "우클릭", Size = new Size(120, 36), Location = new Point(8, 8) };
            var btnHold = new Button { Text = "홀드 2s", Size = new Size(120, 36), Location = new Point(8, 52) };
            var btnClose = new Button { Text = "닫기", Size = new Size(120, 36), Location = new Point(8, 96) };

            btnRightClick.Click += (s, e) => DoRightClick();
            btnHold.Click += (s, e) => DoHold();
            btnClose.Click += (s, e) => Close();

            Controls.Add(btnRightClick);
            Controls.Add(btnHold);
            Controls.Add(btnClose);

            // Position in logical coordinates
            Location = new Point(_x + 10, _y + 10);
        }

        void DoRightClick()
        {
            Program.Log($"OVERLAY: right-click at log=({_x},{_y})");
            SetCursorPos(_x, _y); // logical coords for DPI-unaware app
            Thread.Sleep(100);
            MouseButton(0x0008); // right down
            Thread.Sleep(50);
            MouseButton(0x0010); // right up
            Close();
        }

        void DoHold()
        {
            Program.Log($"OVERLAY: hold 2s at log=({_x},{_y})");
            SetCursorPos(_x, _y);
            Thread.Sleep(100);
            MouseButton(0x0002); // left down
            Thread.Sleep(2000);
            MouseButton(0x0004); // left up
            Program.Log("OVERLAY: hold released");
            Close();
        }

        static void MouseButton(uint flag)
        {
            var input = new INPUT { type = 0, mi_reserved = 0 };
            input.mi.dwFlags = flag;
            uint sent = Program.SendInput(1, new[] { input }, Marshal.SizeOf(typeof(INPUT)));
            Program.Log($"  SendInput: sent={sent}");
        }
    }
}
