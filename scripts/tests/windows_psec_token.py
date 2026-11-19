"""Pinned Windows 11 experiment. Read a suspended Cargo root's primary token.

Use the staged B.bin and cargo.exe from the archived db629ba Cargo pair.
The three processes have no inherited handles and are terminated without resume.
Exit 0 requires both controls and the combined token shape to pass.
"""
import ctypes as c
from ctypes import wintypes as w
import contextlib
import hashlib
import json
import os
from pathlib import Path
import runpy
import sys


def run():
    if len(sys.argv) != 2:
        raise ValueError("Supply the staging directory from the archived Cargo pair")
    stage = Path(sys.argv[1]).resolve(strict=True)
    policy = (stage / "B.bin").read_bytes()
    image = stage / "runtime" / "cargo.exe"
    cwd = stage / "workspace"
    hashes = {"cargo": hashlib.sha256(image.read_bytes()).hexdigest(),
              "policy": hashlib.sha256(policy).hexdigest()}
    if hashes != {"cargo": "c37545ec61d48d31bdefce53280ecab61c4ea54eaace372fac6a0316c6e165d9",
                  "policy": "19605f8e2210a3bbb301447eb8ebab964b9426be9b1564ae460450227d6e9083"}:
        raise RuntimeError("Fixture hash mismatch")
    if str(cwd).encode("utf-8") not in policy:
        raise RuntimeError("Encoded policy paths do not match the fixed Cargo fixture")
    validator = runpy.run_path(str(Path(__file__).resolve().parents[1] / "validate-windows-process-tree.py"))
    api = validator["WindowsHostExecution"]()
    host = api.current_token_metadata()
    api.verify_medium(host, host)
    print(json.dumps({"host": host, "image": str(image), "policy": str(stage / "B.bin"),
                      "hashes": hashes}), flush=True)
    k, a = api.kernel, api.security
    H, D, P = w.HANDLE, w.DWORD, c.c_void_p
    PH, SZ = c.POINTER(H), c.c_size_t
    model = c.WinDLL("processmodel.dll", winmode=0x800, use_last_error=True)

    def bind(dll, name, ret, *args):
        fn = getattr(dll, name)
        fn.restype, fn.argtypes = ret, list(args)
        return fn

    create_psec = bind(model, "CreateProcessSecurityEnvironment", c.c_long, P, D, D, PH)
    close_psec = bind(model, "CloseProcessSecurityEnvironment", None, H)
    make_restricted = bind(a, "CreateRestrictedToken", w.BOOL, H, D, D, P, D, P, D, P, PH)
    parse_sid = bind(a, "ConvertStringSidToSidW", w.BOOL, w.LPCWSTR, c.POINTER(P))
    init = bind(k, "InitializeProcThreadAttributeList", w.BOOL, P, D, D, c.POINTER(SZ))
    update = bind(k, "UpdateProcThreadAttribute", w.BOOL, P, D, SZ, P, SZ, P, P)
    remove = bind(k, "DeleteProcThreadAttributeList", None, P)
    mitigation = bind(k, "GetProcessMitigationPolicy", w.BOOL, H, D, P, SZ)
    launch_args = (w.LPCWSTR, w.LPWSTR, P, P, w.BOOL, D, P, w.LPCWSTR, P, c.POINTER(api.ProcessInformation))
    normal_start = bind(k, "CreateProcessW", w.BOOL, *launch_args)
    as_user = bind(a, "CreateProcessAsUserW", w.BOOL, H, *launch_args)

    class Group(c.Structure):
        _fields_ = [("sid", P), ("attributes", D)]

    class Groups(c.Structure):
        _fields_ = [("count", D), ("first", Group)]

    class ExtendedStartup(c.Structure):
        _fields_ = [("startup", api.StartupInfo), ("attributes", P)]

    def restricted_sids(token):
        buf = api.token_info(token, 11)  # TokenRestrictedSids
        count = D.from_buffer(buf).value
        if count and Groups.first.offset + count * c.sizeof(Group) > len(buf):
            raise RuntimeError("Invalid restricted SID list")
        return [api.sid_text(g.sid) for g in (Group * count).from_address(
            c.addressof(buf) + Groups.first.offset)]

    # Provisioned private Writer group SID from the authorized db629ba Cargo source fixture.
    writer = "S-1-5-21-4275439644-3620842777-1714106797-1061"
    passed = True
    with contextlib.ExitStack() as owned:
        sid, primary, restricted = P(), H(), H()
        api.require(parse_sid(writer, c.byref(sid)), "Parse fixed writer SID")
        owned.callback(k.LocalFree, sid)
        api.require(a.OpenProcessToken(k.GetCurrentProcess(), 0xB, c.byref(primary)), "Open primary token")
        owned.callback(api.close, primary)
        api.require(make_restricted(primary, 0xD, 0, None, 0, None, 1, c.byref(Group(sid, 0)),
                                    c.byref(restricted)), "Create restricted primary token")
        owned.callback(api.close, restricted)
        if restricted_sids(restricted) != [writer]:
            raise RuntimeError("Writer missing from supplied token")
        for label, psec, supplied in (("writer_control", False, True),
                                      ("psec_control", True, False),
                                      ("psec_writer", True, True)):
            record = {"arm": label, "resumed": False, "inherit_handles": False}
            try:
                with contextlib.ExitStack() as arm:
                    startup = ExtendedStartup()
                    startup.startup.cb = c.sizeof(api.StartupInfo)
                    flags = 0x00000004 | 0x08000000 | 0x00000400  # suspended, no window, Unicode env
                    if psec:
                        security = H()
                        status = create_psec(policy, len(policy), 1, c.byref(security))
                        if status != 0 or not security:
                            raise RuntimeError(f"CreateProcessSecurityEnvironment HRESULT {status:#x}")
                        arm.callback(close_psec, security)
                        size = SZ()
                        init(None, 1, 0, c.byref(size))
                        attributes = c.create_string_buffer(size.value)
                        api.require(init(attributes, 1, 0, c.byref(size)), "Initialize attributes")
                        arm.callback(remove, attributes)
                        api.require(update(attributes, 0, 0x20023, c.byref(security), c.sizeof(H), None, None),
                                    "Set security environment")
                        flags |= 0x00080000
                        startup.startup.cb = c.sizeof(startup)
                        startup.attributes = c.addressof(attributes)
                    info = api.ProcessInformation()
                    command = c.create_unicode_buffer(f'"{image}" --version')
                    windows = os.environ["SystemRoot"]
                    scratch = str(stage / "scratch")
                    values = {"HOME": scratch, "LOCALAPPDATA": scratch, "PATH": str(Path(windows) / "System32"),
                              "SystemRoot": windows, "TEMP": scratch, "TMP": scratch,
                              "USERPROFILE": scratch, "WINDIR": windows}
                    env = c.create_unicode_buffer("\0".join(f"{key}={value}" for key, value in values.items()) + "\0")
                    args = (str(image), command, None, None, False, flags, env, str(cwd), c.byref(startup), c.byref(info))
                    ok = as_user(restricted, *args) if supplied else normal_start(*args)
                    api.require(ok, "Create suspended process")
                    arm.callback(api.close, info.thread)
                    arm.callback(api.close, info.process)
                    record["pid"] = info.pid
                    try:
                        token = H()
                        api.require(a.OpenProcessToken(info.process, 8, c.byref(token)), "Read actual root token")
                        arm.callback(api.close, token)
                        meta = api.token_metadata(token)
                        flags_read = D()
                        api.require(mitigation(info.process, 4, c.byref(flags_read), c.sizeof(D)),
                                    "Read Win32k mitigation")
                        record.update(token=meta, restricted_sids=restricted_sids(token),
                                      win32k_flags=flags_read.value)
                        record["passed"] = (not meta["elevated"] and meta["appcontainer"] == psec
                                            and meta["user_sid"] == host["user_sid"]
                                            and meta["session_id"] == host["session_id"]
                                            and flags_read.value & 1 == 0
                                            and (not supplied or record["restricted_sids"] == [writer]))
                    finally:
                        api.require(k.TerminateProcess(info.process, 0xD1), "Reap suspended fixture")
                        record["signaled"] = k.WaitForSingleObject(info.process, 10000) == 0
                        if not record["signaled"]:
                            raise RuntimeError("Fixture cleanup did not complete")
            except (OSError, RuntimeError) as err:
                record.update(error=str(err), passed=False)
            passed &= record["passed"]
            print(json.dumps(record), flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(run())
