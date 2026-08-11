param(
    [switch]$ConfigureOnly,
    [switch]$ServiceDeath
)

$ErrorActionPreference = 'Stop'

if ($ConfigureOnly -and $ServiceDeath) {
    throw 'ConfigureOnly and ServiceDeath are mutually exclusive'
}

if ($ServiceDeath) {
    $child = Start-Process powershell.exe -ArgumentList @(
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy', 'Bypass',
        '-File', $PSCommandPath,
        '-ConfigureOnly'
    ) -Wait -PassThru
    if ($child.ExitCode -ne 0) {
        throw "configuration child failed with exit code $($child.ExitCode)"
    }

    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $client.Connect([System.Net.IPAddress]::Loopback, 18080)
        throw 'connection unexpectedly succeeded after configuring process exited'
    } catch [System.Management.Automation.MethodInvocationException] {
        Write-Output 'WFP service-death fail-closed test passed'
    } finally {
        $client.Dispose()
    }
    exit 0
}

$source = @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.Principal;
using Microsoft.Win32.SafeHandles;

[StructLayout(LayoutKind.Sequential)]
struct Endpoint {
    public ushort Family;
    public ushort Port;
    [MarshalAs(UnmanagedType.ByValArray, SizeConst = 16)]
    public byte[] Address;
    public uint ScopeId;
    [MarshalAs(UnmanagedType.ByValArray, SizeConst = 8)]
    public byte[] Reserved;
}

[StructLayout(LayoutKind.Sequential)]
struct Configuration {
    public uint Version;
    public uint Size;
    public uint LiveServicePid;
    public uint Flags;
    public Endpoint PublicDestination;
    public Endpoint ProxyDestination;
}

public static class WfpSmoke {
    const uint GenericWrite = 0x40000000;
    const uint OpenExisting = 3;
    const uint IoctlSetConfiguration = 0x0012a004;
    const uint QueryRedirectContext = 2550137053;

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern SafeFileHandle CreateFile(
        string name,
        uint access,
        uint share,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flags,
        IntPtr template);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(
        SafeFileHandle device,
        uint controlCode,
        ref Configuration input,
        uint inputSize,
        IntPtr output,
        uint outputSize,
        out uint bytesReturned,
        IntPtr overlapped);

    static Endpoint Ipv4Endpoint(ushort port) {
        var address = new byte[16];
        address[0] = 1;
        address[3] = 127;
        return new Endpoint {
            Family = (ushort)AddressFamily.InterNetwork,
            Port = port,
            Address = address,
            Reserved = new byte[8]
        };
    }

    static ushort ReadUInt16(byte[] bytes, int offset) {
        return (ushort)(bytes[offset] | bytes[offset + 1] << 8);
    }

    static void ValidateRedirectContext(Socket socket) {
        var context = new byte[256];
        var length = socket.IOControl((IOControlCode)QueryRedirectContext, null, context);
        if (length < 16 || context[0] != (byte)'A' || context[1] != (byte)'G' ||
            context[2] != (byte)'W' || context[3] != (byte)'F') {
            throw new InvalidOperationException("redirect context has an invalid header");
        }
        var sockaddrLength = ReadUInt16(context, 8);
        var sidLength = ReadUInt16(context, 10);
        if (ReadUInt16(context, 4) != 1 || ReadUInt16(context, 6) != 1 ||
            sockaddrLength != 16 || length != 16 + sockaddrLength + sidLength) {
            throw new InvalidOperationException("redirect context has invalid ABI fields");
        }
        if (context[16] != (byte)AddressFamily.InterNetwork || context[17] != 0 ||
            context[18] != 0x46 || context[19] != 0xa0 ||
            context[20] != 127 || context[21] != 0 || context[22] != 0 || context[23] != 1) {
            throw new InvalidOperationException("redirect context has the wrong original destination");
        }

        var expectedSid = new byte[WindowsIdentity.GetCurrent().User.BinaryLength];
        WindowsIdentity.GetCurrent().User.GetBinaryForm(expectedSid, 0);
        if (sidLength != expectedSid.Length) {
            throw new InvalidOperationException("redirect context SID has the wrong length");
        }
        for (var index = 0; index < expectedSid.Length; ++index) {
            if (context[16 + sockaddrLength + index] != expectedSid[index]) {
                throw new InvalidOperationException("redirect context SID does not match the initiating account");
            }
        }
    }

    public static void Run(bool validateRedirect) {
        var size = (uint)Marshal.SizeOf(typeof(Configuration));
        if (size != 80) {
            throw new InvalidOperationException(
                string.Format("configuration ABI size is {0}, expected 80", size));
        }

        TcpListener proxy = null;
        if (validateRedirect) {
            proxy = new TcpListener(IPAddress.Loopback, 18081);
            proxy.Start();
        }
        using (var device = CreateFile(
            @"\\.\AGWfp",
            GenericWrite,
            0,
            IntPtr.Zero,
            OpenExisting,
            0,
            IntPtr.Zero)) {
            if (device.IsInvalid) {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "open WFP control device");
            }

            var configuration = new Configuration {
                Version = 1,
                Size = size,
                LiveServicePid = (uint)Process.GetCurrentProcess().Id,
                PublicDestination = Ipv4Endpoint(18080),
                ProxyDestination = Ipv4Endpoint(18081)
            };
            uint returned;
            if (!DeviceIoControl(
                device,
                IoctlSetConfiguration,
                ref configuration,
                size,
                IntPtr.Zero,
                0,
                out returned,
                IntPtr.Zero)) {
                var error = Marshal.GetLastWin32Error();
                throw new InvalidOperationException(
                    string.Format("configure WFP driver failed with Win32 error {0}", error),
                    new Win32Exception(error));
            }
            if (returned != 0) {
                throw new InvalidOperationException(
                    string.Format("configuration IOCTL returned {0} bytes", returned));
            }
            if (DeviceIoControl(
                device,
                IoctlSetConfiguration,
                ref configuration,
                size,
                IntPtr.Zero,
                0,
                out returned,
                IntPtr.Zero)) {
                throw new InvalidOperationException("WFP driver accepted a second configuration");
            }

            if (validateRedirect) {
                using (var client = new TcpClient()) {
                    client.Connect(IPAddress.Loopback, 18080);
                    using (var redirected = proxy.AcceptSocket()) {
                        ValidateRedirectContext(redirected);
                    }
                }
            }
        }
        if (proxy != null) {
            proxy.Stop();
        }
    }
}
'@

Add-Type -TypeDefinition $source -Language CSharp
[WfpSmoke]::Run(-not $ConfigureOnly)
if (-not $ConfigureOnly) {
    Write-Output 'WFP configuration smoke test passed'
}