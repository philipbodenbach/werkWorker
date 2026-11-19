"""Isolate mandatory write-up on disposable Windows files. Run through the ordinary-host verifier."""
import ctypes as c
from ctypes import wintypes as w
import importlib.util
import json
from pathlib import Path
import tempfile

driver_path = Path(__file__).resolve().parents[2] / "scripts/validate-windows-process-tree.py"
spec = importlib.util.spec_from_file_location("native_validation", driver_path)
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

READ = 0x00100001  # SYNCHRONIZE | FILE_READ_DATA
READ_WRITE = 0x00100003  # additionally FILE_WRITE_DATA, as in the measured bcrypt open
LABEL = 0x10  # LABEL_SECURITY_INFORMATION
QUERY = 0x8  # TOKEN_QUERY
INVALID_HANDLE = c.c_void_p(-1).value


class MandatoryLabel(c.Structure):
    _fields_ = [("sid", c.c_void_p), ("attributes", w.DWORD)]


def observe():
    host = driver.WindowsHostExecution()
    sec, kernel = host.security, host.kernel
    ptr = c.c_void_p
    by = c.byref
    sec.ConvertStringSidToSidW.argtypes = [w.LPCWSTR, c.POINTER(ptr)]
    sec.SetTokenInformation.argtypes = [w.HANDLE, c.c_int, ptr, w.DWORD]
    sec.GetLengthSid.argtypes = [ptr]
    sec.SetThreadToken.argtypes = [ptr, w.HANDLE]
    sec.OpenThreadToken.argtypes = [w.HANDLE, w.DWORD, w.BOOL, c.POINTER(w.HANDLE)]
    kernel.GetCurrentThread.restype = w.HANDLE
    kernel.GetVolumePathNameW.argtypes = [w.LPCWSTR, w.LPWSTR, w.DWORD]
    kernel.GetVolumeInformationW.argtypes = [w.LPCWSTR, w.LPWSTR, w.DWORD, ptr, ptr, ptr, w.LPWSTR, w.DWORD]
    sec.GetNamedSecurityInfoW.argtypes = [w.LPCWSTR, c.c_int, w.DWORD, ptr, ptr, ptr, ptr, c.POINTER(ptr)]
    sec.ConvertSecurityDescriptorToStringSecurityDescriptorW.argtypes = [
        ptr, w.DWORD, w.DWORD, c.POINTER(w.LPWSTR), ptr]

    def checked_close(handle):
        host.require(kernel.CloseHandle(handle), "Close diagnostic handle")

    def thread_token():
        handle = w.HANDLE()
        if sec.OpenThreadToken(kernel.GetCurrentThread(), QUERY, True, by(handle)):
            return handle
        if c.get_last_error() != 1008:  # ERROR_NO_TOKEN
            host.require(False, "Read effective thread token")
        return None

    def require_process_identity():
        handle = thread_token()
        if handle is not None:
            checked_close(handle)
            raise RuntimeError("An unexpected caller impersonation is active")

    def volume_type(path):
        volume = c.create_unicode_buffer(32768)
        filesystem = c.create_unicode_buffer(64)
        host.require(kernel.GetVolumePathNameW(str(path), volume, len(volume)), "Resolve fixture volume")
        host.require(kernel.GetVolumeInformationW(volume, None, 0, None, None, None, filesystem, len(filesystem)),
                     "Read fixture filesystem type")
        return filesystem.value

    def create_fixture(path, target_sid, user_sid):
        descriptor = ptr()
        sddl = f"D:P(A;;FA;;;SY)(A;;FA;;;{user_sid})S:(ML;;NW;;;{target_sid})"
        host.require(sec.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, 1, by(descriptor), None), "Construct private fixture security")
        try:
            attrs = host.SecurityAttributes(c.sizeof(host.SecurityAttributes), descriptor, False)
            handle = kernel.CreateFileW(str(path), READ_WRITE, 7, by(attrs), 1, 0x80, None)  # CREATE_NEW
            host.require(handle != INVALID_HANDLE, "Create private file")
            checked_close(handle)
        finally:
            kernel.LocalFree(descriptor)

        # Read back both security components on the created file.
        actual = ptr()
        security_info = LABEL | 0x4  # mandatory label and DACL
        result = sec.GetNamedSecurityInfoW(str(path), 1, security_info, None, None, None, None, by(actual))
        if result:
            raise OSError(result, "Read private file descriptor")
        text = w.LPWSTR()
        try:
            host.require(sec.ConvertSecurityDescriptorToStringSecurityDescriptorW(
                actual, 1, security_info, by(text), None), "Read private file descriptor")
            dacl, separator, sacl = text.value.partition("S:")
            if not separator or not dacl.startswith("D:P") or dacl.count("(") != 2 or any(
                    f"(A;;FA;;;{who})" not in dacl for who in ("SY", user_sid)):
                raise RuntimeError("Unexpected fixture DACL")
            return "S:" + sacl
        finally:
            if text:
                kernel.LocalFree(text)
            kernel.LocalFree(actual)

    def open_existing(path, mask):
        handle = kernel.CreateFileW(str(path), mask, 7, None, 3, 0x80, None)  # OPEN_EXISTING
        if handle == INVALID_HANDLE:
            return c.get_last_error()
        checked_close(handle)
        return 0

    def measure(primary, rid, files):
        token, sid = w.HANDLE(), ptr()
        # Only a test thread impersonates this copy. The process primary token stays Medium.
        host.require(sec.DuplicateTokenEx(primary, QUERY | 0x4 | 0x80, None, 2, 2, by(token)),
                     "Duplicate an impersonation token")
        try:
            host.require(sec.ConvertStringSidToSidW(f"S-1-16-{rid}", by(sid)), "Make integrity SID")
            label = MandatoryLabel(sid, 0x20)  # SE_GROUP_INTEGRITY
            host.require(sec.SetTokenInformation(token, 25, by(label), c.sizeof(label) + sec.GetLengthSid(sid)),
                         "Lower the copied token's integrity")
            host.require(sec.SetThreadToken(None, token), "Impersonate reduced test token")
            try:
                effective = thread_token()
                if effective is None:
                    raise RuntimeError("Missing effective test token")
                try:
                    info = host.token_metadata(effective)
                    policy = w.DWORD.from_buffer(host.token_info(effective, 27)).value
                    if info["integrity_rid"] != rid or info["elevated"] or not (policy & 1):
                        raise RuntimeError("Unexpected effective token")
                    return {"effective_integrity_rid": rid, "mandatory_policy": policy,
                            "token_type": info["token_type"], "elevated": info["elevated"],
                            "opens": {name: {hex(mask): open_existing(path, mask)
                                             for mask in (READ, READ_WRITE)} for name, path in files.items()}}
                finally:
                    checked_close(effective)
            finally:
                if not sec.RevertToSelf():
                    kernel.ExitProcess(125)
        finally:
            if sid:
                kernel.LocalFree(sid)
            checked_close(token)

    require_process_identity()
    primary = w.HANDLE()
    host.require(sec.OpenProcessToken(kernel.GetCurrentProcess(), QUERY | 2, by(primary)), "Read primary token")
    try:
        info = host.token_metadata(primary)
        host.verify_medium(info, info)
        with tempfile.TemporaryDirectory(prefix="ww-mandatory-file-") as directory:
            root = Path(directory)
            filesystem = volume_type(root)
            if filesystem != "NTFS":
                raise RuntimeError(f"This control requires NTFS, got {filesystem}")
            files = {name: root / name for name in ("low", "untrusted")}
            labels = {key: create_fixture(path, "S-1-16-4096" if key == "low" else "S-1-16-0", info["user_sid"])
                      for key, path in files.items()}
            if "ML;;NW;;;LW)" not in labels["low"] or not any(
                    item in labels["untrusted"] for item in ("ML;;NW;;;UN)", "ML;;NW;;;S-1-16-0)")):
                raise RuntimeError(f"Unexpected file labels {labels}")
            rows = [measure(primary, rid, files) for rid in (4096, 0)]
            require_process_identity()
            intact = all(path.read_bytes() == b"" for path in files.values())
        expected = [{"low": {hex(READ): 0, hex(READ_WRITE): 0}, "untrusted": {hex(READ): 0, hex(READ_WRITE): 0}},
                    {"low": {hex(READ): 0, hex(READ_WRITE): 5}, "untrusted": {hex(READ): 0, hex(READ_WRITE): 0}}]
        if [row["opens"] for row in rows] != expected or not intact or root.exists():
            raise RuntimeError(f"Unexpected measurement or cleanup: {rows}")
        return {"filesystem": filesystem, "dacl_readback": "Protected, exactly full access for SYSTEM and test account",
                "host_primary_integrity_rid": info["integrity_rid"], "file_labels": labels,
                "effective_thread_tokens": rows, "no_data_changed": intact, "fresh_directory_removed": True,
                "thread_reverted": True, "scope": "Owned NTFS file handles. OS error 5 is access denied. No device, Registry or payload-tree operations."}
    finally:
        checked_close(primary)


if __name__ == "__main__":
    print(json.dumps(observe(), sort_keys=True))
