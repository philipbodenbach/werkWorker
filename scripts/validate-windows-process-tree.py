#!/usr/bin/env python3
"""Run native tree acceptance with a controlled non-loopback TCP witness.

This helper does not provision accounts, modify policy, or bypass qualification.
Use it from an x64 MSVC shell with an explicitly chosen source/installation.
"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import threading


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class WindowsHostExecution:
    """Start trusted acceptance hosts as the same user's ordinary Medium token.

    Provisioning may run elevated. The native suite and installed CLI must not
    inherit those administrative rights. A linked-token launch has an explicit
    zero-handle bridge and is inspected while suspended before any user code runs.
    """

    def __init__(self):
        if os.name != "nt":
            raise RuntimeError("Native Windows host-token verification requires Windows")
        import ctypes
        from ctypes import wintypes
        self.ctypes = ctypes
        self.w = wintypes
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.security = ctypes.WinDLL("advapi32", use_last_error=True)

        class SecurityAttributes(ctypes.Structure):
            _fields_ = [("length", wintypes.DWORD), ("descriptor", ctypes.c_void_p),
                        ("inherit", wintypes.BOOL)]

        class StartupInfo(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("reserved", wintypes.LPWSTR),
                        ("desktop", wintypes.LPWSTR), ("title", wintypes.LPWSTR),
                        ("x", wintypes.DWORD), ("y", wintypes.DWORD),
                        ("width", wintypes.DWORD), ("height", wintypes.DWORD),
                        ("chars_x", wintypes.DWORD), ("chars_y", wintypes.DWORD),
                        ("fill", wintypes.DWORD), ("flags", wintypes.DWORD),
                        ("show", wintypes.WORD), ("reserved_count", wintypes.WORD),
                        ("reserved_bytes", ctypes.c_void_p),
                        ("stdin", wintypes.HANDLE), ("stdout", wintypes.HANDLE),
                        ("stderr", wintypes.HANDLE)]

        class ProcessInformation(ctypes.Structure):
            _fields_ = [("process", wintypes.HANDLE), ("thread", wintypes.HANDLE),
                        ("pid", wintypes.DWORD), ("tid", wintypes.DWORD)]

        class BasicJobLimits(ctypes.Structure):
            _fields_ = [("process_time", ctypes.c_longlong), ("job_time", ctypes.c_longlong),
                        ("flags", wintypes.DWORD), ("minimum_ws", ctypes.c_size_t),
                        ("maximum_ws", ctypes.c_size_t), ("processes", wintypes.DWORD),
                        ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                        ("scheduling", wintypes.DWORD)]

        class ExtendedJobLimits(ctypes.Structure):
            _fields_ = [("basic", BasicJobLimits), ("io_counters", ctypes.c_ulonglong * 6),
                        ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
                        ("peak_process_memory", ctypes.c_size_t),
                        ("peak_job_memory", ctypes.c_size_t)]

        class JobAccounting(ctypes.Structure):
            _fields_ = [("user_time", ctypes.c_longlong), ("kernel_time", ctypes.c_longlong),
                        ("period_user_time", ctypes.c_longlong),
                        ("period_kernel_time", ctypes.c_longlong),
                        ("page_faults", wintypes.DWORD), ("total_processes", wintypes.DWORD),
                        ("active_processes", wintypes.DWORD),
                        ("terminated_processes", wintypes.DWORD)]

        self.ExtendedJobLimits = ExtendedJobLimits
        self.JobAccounting = JobAccounting
        self.SecurityAttributes = SecurityAttributes
        self.StartupInfo = StartupInfo
        self.ProcessInformation = ProcessInformation
        self.medium_source = None
        self.last_metadata = {"mode": "launch_refused"}
        pointer = ctypes.c_void_p
        handle_pointer = ctypes.POINTER(wintypes.HANDLE)
        dword_pointer = ctypes.POINTER(wintypes.DWORD)
        self.kernel.GetCurrentProcess.argtypes = []
        self.kernel.GetCurrentProcess.restype = wintypes.HANDLE
        self.kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.kernel.OpenProcess.restype = wintypes.HANDLE
        self.kernel.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                                          wintypes.LPWSTR,
                                                          ctypes.POINTER(wintypes.DWORD)]
        self.kernel.QueryFullProcessImageNameW.restype = wintypes.BOOL
        self.kernel.GetSystemWindowsDirectoryW.argtypes = [wintypes.LPWSTR, wintypes.UINT]
        self.kernel.GetSystemWindowsDirectoryW.restype = wintypes.UINT
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel.CloseHandle.restype = wintypes.BOOL
        self.kernel.GetHandleInformation.argtypes = [wintypes.HANDLE, dword_pointer]
        self.kernel.GetHandleInformation.restype = wintypes.BOOL
        self.kernel.LocalFree.argtypes = [pointer]
        self.kernel.LocalFree.restype = pointer
        self.security.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
            wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(pointer), dword_pointer]
        self.security.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = wintypes.BOOL
        self.kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD,
                                            wintypes.DWORD, pointer, wintypes.DWORD,
                                            wintypes.DWORD, wintypes.HANDLE]
        self.kernel.CreateFileW.restype = wintypes.HANDLE
        self.security.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                                   handle_pointer]
        self.security.OpenProcessToken.restype = wintypes.BOOL
        self.security.GetTokenInformation.argtypes = [wintypes.HANDLE, ctypes.c_int,
                                                       pointer, wintypes.DWORD,
                                                       dword_pointer]
        self.security.GetTokenInformation.restype = wintypes.BOOL
        self.security.ConvertSidToStringSidW.argtypes = [pointer,
                                                         ctypes.POINTER(wintypes.LPWSTR)]
        self.security.ConvertSidToStringSidW.restype = wintypes.BOOL
        self.security.IsValidSid.argtypes = [pointer]
        self.security.IsValidSid.restype = wintypes.BOOL
        self.security.GetSidSubAuthorityCount.argtypes = [pointer]
        self.security.GetSidSubAuthorityCount.restype = ctypes.POINTER(ctypes.c_ubyte)
        self.security.GetSidSubAuthority.argtypes = [pointer, wintypes.DWORD]
        self.security.GetSidSubAuthority.restype = dword_pointer
        self.security.DuplicateTokenEx.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                                   pointer, ctypes.c_int, ctypes.c_int,
                                                   handle_pointer]
        self.security.DuplicateTokenEx.restype = wintypes.BOOL
        self.security.CreateProcessWithTokenW.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                                           wintypes.LPCWSTR, wintypes.LPWSTR,
                                                           wintypes.DWORD, pointer,
                                                           wintypes.LPCWSTR, pointer,
                                                           ctypes.POINTER(ProcessInformation)]
        self.security.CreateProcessWithTokenW.restype = wintypes.BOOL
        self.kernel.CreateJobObjectW.argtypes = [pointer, wintypes.LPCWSTR]
        self.kernel.CreateJobObjectW.restype = wintypes.HANDLE
        self.kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int,
                                                       pointer, wintypes.DWORD]
        self.kernel.SetInformationJobObject.restype = wintypes.BOOL
        self.kernel.QueryInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int,
                                                         pointer, wintypes.DWORD, pointer]
        self.kernel.QueryInformationJobObject.restype = wintypes.BOOL
        self.kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        self.kernel.AssignProcessToJobObject.restype = wintypes.BOOL
        self.kernel.IsProcessInJob.argtypes = [wintypes.HANDLE, wintypes.HANDLE,
                                              ctypes.POINTER(wintypes.BOOL)]
        self.kernel.IsProcessInJob.restype = wintypes.BOOL
        self.kernel.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.kernel.TerminateJobObject.restype = wintypes.BOOL
        self.kernel.GetProcessHandleCount.argtypes = [wintypes.HANDLE, dword_pointer]
        self.kernel.GetProcessHandleCount.restype = wintypes.BOOL
        self.kernel.ResumeThread.argtypes = [wintypes.HANDLE]
        self.kernel.ResumeThread.restype = wintypes.DWORD
        self.kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        self.kernel.WaitForSingleObject.restype = wintypes.DWORD
        self.kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, dword_pointer]
        self.kernel.GetExitCodeProcess.restype = wintypes.BOOL
        self.kernel.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.kernel.TerminateProcess.restype = wintypes.BOOL

    def require(self, ok, operation):
        if not ok:
            error = self.ctypes.get_last_error()
            raise OSError(error, f"{operation}: {self.ctypes.FormatError(error).strip()}")

    def close(self, handle):
        if handle:
            self.kernel.CloseHandle(handle)

    def token_info(self, token, kind):
        c, w = self.ctypes, self.w
        required = w.DWORD()
        self.security.GetTokenInformation(token, kind, None, 0, c.byref(required))
        if not 0 < required.value <= 65536:
            self.require(False, f"Query host token information size ({kind})")
        buffer = c.create_string_buffer(required.value)
        self.require(self.security.GetTokenInformation(token, kind, buffer,
                                                       len(buffer), c.byref(required)),
                     f"Read host token information ({kind})")
        return buffer

    def sid_text(self, sid):
        c, w = self.ctypes, self.w
        self.require(self.security.IsValidSid(sid), "Validate host identity SID")
        text = w.LPWSTR()
        self.require(self.security.ConvertSidToStringSidW(sid, c.byref(text)),
                     "Format host identity SID")
        try:
            return text.value
        finally:
            self.kernel.LocalFree(c.cast(text, c.c_void_p))

    def token_metadata(self, token):
        c, w = self.ctypes, self.w
        user = self.token_info(token, 1)  # TokenUser
        sid = c.cast(user, c.POINTER(c.c_void_p)).contents.value
        integrity = self.token_info(token, 25)  # TokenIntegrityLevel
        integrity_sid = c.cast(integrity, c.POINTER(c.c_void_p)).contents.value
        self.require(self.security.IsValidSid(integrity_sid), "Validate host integrity SID")
        count = self.security.GetSidSubAuthorityCount(integrity_sid).contents.value
        if not 0 < count <= 15:
            raise RuntimeError("Invalid native host integrity SID")
        rid = self.security.GetSidSubAuthority(integrity_sid, count - 1).contents.value
        read_uint = lambda kind: c.cast(self.token_info(token, kind),
                                        c.POINTER(w.DWORD)).contents.value
        statistics = self.token_info(token, 10)  # TOKEN_STATISTICS.AuthenticationId
        authentication_id = int.from_bytes(statistics.raw[8:16], "little")
        return {"user_sid": self.sid_text(sid), "elevated": bool(read_uint(20)),
                "authentication_id": f"0x{authentication_id:016x}",
                "integrity_rid": rid, "integrity_sid": self.sid_text(integrity_sid),
                "elevation_type": read_uint(18), "session_id": read_uint(12),
                "token_type": read_uint(8), "appcontainer": bool(read_uint(29))}

    @staticmethod
    def verify_medium(metadata, expected, primary=True):
        if (metadata["user_sid"] != expected["user_sid"]
                or metadata["session_id"] != expected["session_id"]
                or metadata["elevated"] or metadata["integrity_rid"] != 0x2000
                or metadata["token_type"] not in ((1,) if primary else (1, 2))
                or metadata["appcontainer"]):
            raise RuntimeError("Native acceptance requires the same host SID/session with "
                               f"a non-elevated primary Medium token: {metadata}")

    def verify_not_inherited(self, handle, name):
        flags = self.w.DWORD()
        self.require(self.kernel.GetHandleInformation(handle, self.ctypes.byref(flags)),
                     f"Read {name} handle inheritance")
        if flags.value & 1:
            raise RuntimeError(f"{name} handle must never be inheritable")

    def explorer_medium(self, linked_metadata, expected):
        """Query only the current shell, never search or modify other users' tokens.

        Without SeTcbPrivilege Windows can return an Identification-only linked
        token. The pinned system Explorer must have that exact linked logon's
        AuthenticationId as well as the same SID, session and Medium integrity.
        """
        c, w = self.ctypes, self.w
        shell = c.WinDLL("user32", use_last_error=True)
        shell.GetShellWindow.argtypes = []
        shell.GetShellWindow.restype = w.HWND
        shell.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
        shell.GetWindowThreadProcessId.restype = w.DWORD
        window = shell.GetShellWindow()
        if not window:
            raise RuntimeError("No system Explorer shell is available for the verified linked "
                               "Medium logon; run acceptance from an unelevated host shell")
        pid = w.DWORD()
        self.require(shell.GetWindowThreadProcessId(window, c.byref(pid)),
                     "Identify only the ordinary host's Explorer shell")
        process = self.kernel.OpenProcess(0x1000, False, pid.value)
        self.require(process, "Pin the system Explorer process for read-only token query")
        original = w.HANDLE()
        duplicate = w.HANDLE()
        try:
            self.verify_not_inherited(process, "Pinned Explorer process")
            image = c.create_unicode_buffer(32768)
            length = w.DWORD(len(image))
            self.require(self.kernel.QueryFullProcessImageNameW(process, 0, image,
                                                                 c.byref(length)),
                         "Verify the pinned Explorer image identity")
            windows = c.create_unicode_buffer(32768)
            count = self.kernel.GetSystemWindowsDirectoryW(windows, len(windows))
            if not 0 < count < len(windows):
                self.require(False, "Resolve the trusted system Windows directory")
            expected_image = (Path(windows.value) / "explorer.exe").resolve(strict=True)
            actual_image = Path(image.value).resolve(strict=True)
            if os.path.normcase(str(actual_image)) != os.path.normcase(str(expected_image)):
                raise RuntimeError(f"The shell is not the expected system Explorer: {actual_image}")
            self.require(self.security.OpenProcessToken(process, 0x000A, c.byref(original)),
                         "Query and duplicate only the same host's system Explorer token")
            self.verify_not_inherited(original, "Explorer source token")
            metadata = self.token_metadata(original)
            self.verify_medium(metadata, expected)
            if (metadata["authentication_id"] != linked_metadata["authentication_id"]
                    or metadata["elevation_type"] != linked_metadata["elevation_type"]):
                raise RuntimeError("System Explorer does not belong to the exact linked Medium logon")
            self.require(self.security.DuplicateTokenEx(original, 0x02000000, None, 2, 1,
                                                         c.byref(duplicate)),
                         "Duplicate the verified linked-logon Explorer primary token")
            self.verify_not_inherited(duplicate, "Medium host token")
            duplicated = self.token_metadata(duplicate)
            self.verify_medium(duplicated, expected)
            if duplicated["authentication_id"] != linked_metadata["authentication_id"]:
                raise RuntimeError("Duplicated Explorer token changed linked-logon identity")
            self.medium_source = {"kind": "system_explorer_matching_linked_logon",
                                  "process_id": pid.value, "image": str(actual_image),
                                  "linked_token": linked_metadata, "primary_token": duplicated}
            return duplicate
        except BaseException:
            self.close(duplicate)
            raise
        finally:
            self.close(original)
            self.close(process)

    def linked_medium(self, current, expected):
        c, w = self.ctypes, self.w
        try:
            data = self.token_info(current, 19)  # TokenLinkedToken
        except OSError as error:
            raise RuntimeError("The administrative shell has no usable linked Medium token; "
                               "run native acceptance from an unelevated shell of the "
                               "same host account. No elevation or runas dialog is used.") from error
        linked = c.cast(data, c.POINTER(w.HANDLE)).contents.value
        if not linked:
            raise RuntimeError("The administrative shell returned no linked Medium token")
        duplicate = w.HANDLE()
        try:
            metadata = self.token_metadata(linked)
            self.verify_medium(metadata, expected, primary=False)
            if metadata["token_type"] == 2:
                level = c.cast(self.token_info(linked, 9), c.POINTER(w.DWORD)).contents.value
                if level < 2:
                    return self.explorer_medium(metadata, expected)
            self.require(self.security.DuplicateTokenEx(linked, 0x02000000, None, 2, 1,
                                                         c.byref(duplicate)),
                         "Duplicate the same host's linked Medium primary token")
            self.verify_not_inherited(duplicate, "Medium host token")
            primary = self.token_metadata(duplicate)
            self.verify_medium(primary, expected)
            if primary["authentication_id"] != metadata["authentication_id"]:
                raise RuntimeError("Duplicated token changed linked-logon identity")
            self.medium_source = {"kind": "linked_token", "linked_token": metadata,
                                  "primary_token": primary}
            return duplicate
        except BaseException:
            self.close(duplicate)
            raise
        finally:
            self.close(linked)

    def write_request(self, path, request, host_sid):
        """Create the command request with a trusted DACL before publishing bytes."""
        import msvcrt
        c = self.ctypes
        descriptor = c.c_void_p()
        self.require(self.security.ConvertStringSecurityDescriptorToSecurityDescriptorW(
            f"D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{host_sid})", 1,
            c.byref(descriptor), None), "Build the ordinary host command request DACL")
        handle = None
        try:
            attributes = self.SecurityAttributes(c.sizeof(self.SecurityAttributes), descriptor, False)
            handle = self.kernel.CreateFileW(str(path), 0x40000000, 1,
                                              c.byref(attributes), 1, 0x80, None)
            if handle == c.c_void_p(-1).value:
                handle = None
                self.require(False, "Create a new protected ordinary command request")
            fd = msvcrt.open_osfhandle(handle, os.O_WRONLY | os.O_BINARY)
            handle = None  # The fd now owns the handle.
            with os.fdopen(fd, "wb") as output:
                output.write((json.dumps(request, indent=2) + "\n").encode("utf-8"))
                output.flush()
                os.fsync(output.fileno())
        finally:
            self.close(handle)
            self.kernel.LocalFree(descriptor)

    def create_bridge_job(self):
        """Own the trusted command tree; apply no UI or resource-budget changes."""
        c, w = self.ctypes, self.w
        job = self.kernel.CreateJobObjectW(None, None)
        self.require(job, "Create the ordinary host cleanup Job")
        try:
            self.verify_not_inherited(job, "Ordinary host cleanup Job")
            limits = self.ExtendedJobLimits()
            limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE only.
            self.require(self.kernel.SetInformationJobObject(job, 9, c.byref(limits),
                                                              c.sizeof(limits)),
                         "Set only kill-on-close on the ordinary host cleanup Job")
            actual = self.ExtendedJobLimits()
            self.require(self.kernel.QueryInformationJobObject(job, 9, c.byref(actual),
                                                                c.sizeof(actual), None),
                         "Verify the exact ordinary host cleanup Job limits")
            ui = w.DWORD()
            self.require(self.kernel.QueryInformationJobObject(job, 4, c.byref(ui),
                                                                c.sizeof(ui), None),
                         "Verify the ordinary host cleanup Job has no UI restrictions")
            if (actual.basic.flags != 0x2000 or actual.basic.processes != 0
                    or actual.basic.process_time != 0 or actual.basic.job_time != 0
                    or actual.process_memory != 0 or actual.job_memory != 0 or ui.value != 0):
                raise RuntimeError("The ordinary host cleanup Job unexpectedly changes UI or budgets")
            return job
        except BaseException:
            self.close(job)
            raise

    def bridge_job_active(self, job):
        info = self.JobAccounting()
        self.require(self.kernel.QueryInformationJobObject(job, 1, self.ctypes.byref(info),
                                                            self.ctypes.sizeof(info), None),
                     "Read the exact ordinary host cleanup Job process count")
        return info.active_processes

    def drain_bridge_job(self, job):
        import time
        started = time.monotonic()
        before = self.bridge_job_active(job)
        self.require(self.kernel.TerminateJobObject(job, 125),
                     "Terminate the complete ordinary host command tree")
        deadline = started + 30
        while self.bridge_job_active(job) != 0:
            if time.monotonic() >= deadline:
                raise RuntimeError("Ordinary host command-tree cleanup is unconfirmed; "
                                   "the owned Job is still active")
            time.sleep(0.01)
        return {"active_before_cleanup": before, "active_after_cleanup": 0,
                "confirmed": True, "elapsed_ms": round((time.monotonic() - started) * 1000)}

    def launch_medium(self, command, directory, environment, output, token, launcher):
        import secrets
        import sys
        c, w = self.ctypes, self.w
        parts = [str(part) for part in command]
        if not parts:
            raise ValueError("Empty native acceptance command")
        cwd = str(Path(directory).resolve(strict=True))
        log = Path(output.name).resolve(strict=True)
        nonce = secrets.token_hex(16)
        task = log.parent / f"ordinary-command-{nonce}.json"
        result_file = log.parent / f"ordinary-result-{nonce}.json"
        primary = self.medium_source["primary_token"]
        # The request contains only the acceptance command, paths and identity.
        # Environment values and credentials stay exclusively in API memory.
        request = {"nonce": nonce, "command": parts, "cwd": cwd,
                   "log": str(log), "result": str(result_file), "expected_host": primary}
        self.write_request(task, request, launcher["user_sid"])
        helper = Path(__file__).resolve(strict=True)
        executable = str(Path(sys.executable).resolve(strict=True))
        arguments = [executable, str(helper), "--ordinary-command", str(task), nonce]
        command_line = subprocess.list2cmdline(arguments)
        if len(command_line) >= 1024:
            raise RuntimeError("Choose a shorter evidence path for linked-token bridge startup "
                               "(CreateProcessWithTokenW command line limit is 1024 characters)")
        pairs = []
        for key, value in sorted(environment.items(), key=lambda item: item[0].casefold()):
            if not key or "\0" in key or "\0" in value or "=" in key[1:]:
                raise ValueError("Invalid native acceptance environment entry")
            pairs.append(f"{key}={value}")
        env = c.create_unicode_buffer("\0".join(pairs) + "\0\0")
        cmd = c.create_unicode_buffer(command_line)
        startup = self.StartupInfo()
        startup.cb = c.sizeof(startup)
        process = self.ProcessInformation()
        completed = False
        job = None
        job_metadata = {"kill_on_close": True, "ui_restrictions": 0,
                        "resource_limit_flags": 0, "membership_verified": False,
                        "cleanup": {"confirmed": False}}
        try:
            job = self.create_bridge_job()
            # No STARTF_USESTDHANDLES, no extended attributes, no inherited
            # handles. The verified Medium helper opens its own logs after resume.
            self.require(self.security.CreateProcessWithTokenW(
                token, 0, executable, cmd, 0x08000404, env, cwd,
                c.byref(startup), c.byref(process)),
                "Create suspended ordinary Python host with zero inherited handles")
            self.verify_not_inherited(process.process, "Native host process")
            self.verify_not_inherited(process.thread, "Native host thread")
            self.require(self.kernel.AssignProcessToJobObject(job, process.process),
                         "Assign the suspended ordinary host to its exact cleanup Job")
            member = w.BOOL()
            self.require(self.kernel.IsProcessInJob(process.process, job, c.byref(member)),
                         "Verify the suspended host belongs to the owned cleanup Job")
            if not member.value:
                raise RuntimeError("The suspended ordinary host is outside its cleanup Job")
            job_metadata["membership_verified"] = True
            count = w.DWORD()
            self.require(self.kernel.GetProcessHandleCount(process.process, c.byref(count)),
                         "Verify zero handles in the suspended ordinary Python host")
            if count.value != 0:
                raise RuntimeError("Suspended ordinary host unexpectedly inherited handles: "
                                   f"expected 0, observed {count.value}")
            child_token = w.HANDLE()
            self.require(self.security.OpenProcessToken(process.process, 8, c.byref(child_token)),
                         "Inspect the suspended ordinary Python host token")
            try:
                actual = self.token_metadata(child_token)
                self.verify_medium(actual, launcher)
                if actual["authentication_id"] != primary["authentication_id"]:
                    raise RuntimeError("Suspended ordinary host changed linked-logon identity")
            finally:
                self.close(child_token)
            metadata = {"mode": "linked_medium_bridge", "launcher": launcher,
                        "execution": actual, "verified_before_resume": True,
                        "inherited_handle_count": 0, "suspended_handle_count": count.value,
                        "inherited_handles": [], "token_source": self.medium_source,
                        "creation_api": "CreateProcessWithTokenW", "task": str(task),
                        "result": str(result_file), "nonce": nonce,
                        "outer_job": job_metadata}
            self.last_metadata = metadata
            if self.kernel.ResumeThread(process.thread) != 1:
                self.require(False, "Resume verified ordinary Python host")
            metadata["resumed"] = True
            wait = self.kernel.WaitForSingleObject(process.process, 0xFFFFFFFF)
            if wait != 0:
                self.require(False, "Wait for the ordinary Python host")
            bridge_exit = w.DWORD()
            self.require(self.kernel.GetExitCodeProcess(process.process, c.byref(bridge_exit)),
                         "Read ordinary Python host exit code")
            completed = True
            metadata["bridge_exit_code"] = bridge_exit.value
            if not result_file.is_file():
                raise RuntimeError("Ordinary host exited without its nonce-bound command result")
            receipt = json.loads(result_file.read_text(encoding="utf-8"))
            if (receipt.get("nonce") != nonce or receipt.get("command") != parts
                    or receipt.get("cwd") != cwd):
                raise RuntimeError("Ordinary host command-result nonce or command did not match")
            execution = receipt.get("native_host_token")
            if not isinstance(execution, dict):
                raise RuntimeError("Ordinary host result has no verified execution token")
            self.verify_medium(execution, launcher)
            if execution["authentication_id"] != primary["authentication_id"]:
                raise RuntimeError("Ordinary command result changed linked-logon identity")
            metadata["command_host_token"] = execution
            if bridge_exit.value != 0 or receipt.get("error"):
                raise RuntimeError("Ordinary command bridge failed: " + str(receipt.get("error")))
            code = receipt.get("exit_code")
            if not isinstance(code, int) or isinstance(code, bool):
                raise RuntimeError("Ordinary host result has no native command exit code")
            return code, metadata
        finally:
            try:
                if job:
                    # The native command has already returned and performed its
                    # own assertions. This outer drain also covers Python-host
                    # aborts and residual trusted console helpers; it does not
                    # replace any product Job cleanup assertion.
                    job_metadata["cleanup"] = self.drain_bridge_job(job)
            finally:
                try:
                    if process.process and not completed:
                        # Covers failure before assignment as well as aborts.
                        self.kernel.TerminateProcess(process.process, 125)
                        if self.kernel.WaitForSingleObject(process.process, 10000) != 0:
                            raise RuntimeError("Ordinary host process termination is unconfirmed")
                finally:
                    self.close(process.thread)
                    self.close(process.process)
                    self.close(job)

    def run(self, command, directory, environment, output):
        c, w = self.ctypes, self.w
        current = w.HANDLE()
        self.require(self.security.OpenProcessToken(self.kernel.GetCurrentProcess(), 8,
                                                     c.byref(current)),
                     "Inspect the acceptance launcher's host token")
        token = None
        try:
            launcher = self.token_metadata(current)
            self.last_metadata["launcher"] = launcher
            if launcher["elevated"]:
                token = self.linked_medium(current, launcher)
                return self.launch_medium(command, directory, environment, output, token, launcher)
            self.verify_medium(launcher, launcher)
        finally:
            self.close(token)
            self.close(current)
        # A normal host keeps the existing subprocess path. Its unchanged token
        # has been verified before launch, without creating an elevation broker.
        result = subprocess.run(command, cwd=directory, env=environment,
                                stdin=subprocess.DEVNULL, stdout=output,
                                stderr=subprocess.STDOUT, check=False)
        return result.returncode, {"mode": "ordinary_host", "launcher": launcher,
                                   "execution": launcher, "verified_before_resume": False,
                                   "token_source": "unchanged ordinary parent subprocess"}


def execute(command, directory, environment, log):
    executor = None
    with log.open("wb") as output:
        try:
            executor = WindowsHostExecution()
            exit_code, host_token = executor.run(command, directory, environment, output)
        except (OSError, RuntimeError, ValueError) as error:
            exit_code = 125
            host_token = dict(executor.last_metadata if executor else {"mode": "launch_refused"},
                              error=str(error))
            output.seek(0, os.SEEK_END)
            output.write(("Native ordinary-host launch refused: " + str(error) + "\n").encode("utf-8"))
    return {"command": [str(part) for part in command],
            "cwd": str(directory), "exit_code": exit_code,
            "native_host_token": host_token,
            "log": str(log), "log_sha256": digest(log)}

class Witness:
    def __init__(self, address):
        ip = __import__("ipaddress").ip_address(address)
        if ip.is_loopback or ip.is_unspecified:
            raise ValueError("Choose an explicit assigned non-loopback host address")
        self.server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.server.bind((address, 0))
        self.server.listen()
        self.server.settimeout(0.2)
        self.endpoint = "%s:%d" % self.server.getsockname()
        self.closed = threading.Event()
        self.connections = 0
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.closed.is_set():
            try:
                client, _ = self.server.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.connections += 1
            client.close()

    def close(self):
        self.closed.set()
        self.server.close()
        self.thread.join(timeout=1)


def suite(args, environment):
    if not args.bind_address:
        raise ValueError("--bind-address is required for the native suite")
    witness = Witness(args.bind_address)
    try:
        environment.update({
            "REQUIRE_WINDOWS_PROCESS_TREE": "1",
            "WW_WINDOWS_PROCESS_TREE_PROVISION": str(args.manifest),
            "WW_WINDOWS_PROCESS_TREE_CARGO": str(args.manifest.parent / "toolchains/0/bin/cargo.exe"),
            "WW_WINDOWS_PROCESS_TREE_EXTERNAL_ENDPOINT": witness.endpoint,
        })
        command = ["cargo", "test", "--locked", "--offline", "--manifest-path",
                   str(args.source / "Cargo.toml"), "--target-dir", str(args.evidence / "suite-build"),
                   "--test", "windows_process_tree", "--"]
        if args.diagnostic:
            environment["WW_TREE_DIAGNOSTIC_CASE"] = args.diagnostic
            command += ["--ignored", "--exact", "diagnose_native_process_tree_case"]
        command += ["--nocapture", "--test-threads=1"]
        label = "diagnostic-" + args.diagnostic if args.diagnostic else "mandatory-suite"
        result = execute(command, args.source, environment, args.evidence / (label + ".log"))
        result.update({"qualification_suite": not bool(args.diagnostic),
                       "external_witness": witness.endpoint,
                       "witness_connections": witness.connections})
        required = [
            *("NATIVE_TREE_LIFECYCLE_OK " + case for case in
              ("normal", "parent-exit", "timeout", "cancel", "abort", "engine-close")),
            "NATIVE_TREE_CONSOLE_BUDGET_OK console-alloc",
            "NATIVE_TREE_CONSOLE_BUDGET_OK console-child",
            "NATIVE_TREE_UI_BOUNDARIES_OK", "NATIVE_TREE_DNS_BROKER_DENIED_OK",
            "NATIVE_TREE_THREAD_IDENTITY_NETWORK_MATRIX_OK",
            "NATIVE_TREE_HOST_LOGON_SID_BOUNDARY_OK",
            "NATIVE_TREE_FILE_REGISTRY_NETWORK_IPC_HANDLES_OK",
            "NATIVE_TREE_INBOUND_LOOPBACK_OK", "NATIVE_TREE_RESOURCE_OK memory",
            "NATIVE_TREE_RESOURCE_OK cpu", "NATIVE_TREE_BOUNDED_OUTPUT_AND_DRAIN_OK",
            "NATIVE_TREE_OFFLINE_CARGO_OK", "NATIVE_TREE_CRASH_WORKSPACE_BARRIER_OK",
            "NATIVE_TREE_HOST_DEATH_AND_RESTART_DENIAL_OK",
        ] if not args.diagnostic else ["NATIVE_TREE_DIAGNOSTIC_ONLY_PASSED " + args.diagnostic]
        output = Path(result["log"]).read_text(encoding="utf-8", errors="replace")
        result["missing_acceptance_evidence"] = [item for item in required if item not in output]
        result["passed"] = (result["exit_code"] == 0
                            and not result["missing_acceptance_evidence"]
                            and "NOT RUN:" not in output)
        write_json(args.evidence / (label + "-result.json"), result)
        print(json.dumps(result), flush=True)
        return 0 if result["passed"] else 1
    finally:
        witness.close()


def tool_turn(tool, arguments):
    import secrets
    return {"deltas": [{"type": "tool", "index": 0, "id": "installed-" + secrets.token_hex(8),
                        "name": tool, "arguments": json.dumps(arguments)},
                       {"type": "finish", "reason": "tool_calls"}, {"type": "done"}],
            "error": None}


def objects(value):
    if isinstance(value, dict):
        yield value
        for part in value.values():
            yield from objects(part)
    elif isinstance(value, list):
        for part in value:
            yield from objects(part)


def installed_start(args, environment):
    if args.installed_bin is None:
        raise ValueError("--installed-bin is required for installed-start")
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    import secrets
    crate = Path(manifest["workspace"]) / ("installed-offline-cargo-" + secrets.token_hex(8))
    crate.mkdir()  # Every attempt gets a new crate with no prior outputs.
    (crate / "src").mkdir()
    (crate / ".cargo").mkdir()
    linker = Path(manifest["toolchain_roots"][1]) / "link.exe"
    (crate / ".cargo/config.toml").write_text(
        "[target.x86_64-pc-windows-msvc]\nlinker = " + json.dumps(str(linker)) + "\n",
        encoding="utf-8")
    (crate / "Cargo.toml").write_text(
        '[package]\nname="installed-offline-tree"\nversion="0.0.0"\nedition="2024"\n'
        '[lib]\ndoctest=false\n[workspace]\n', encoding="utf-8")
    (crate / "src/lib.rs").write_text(
        '#[test]\nfn installed_native_test() {\n'
        'assert_eq!(2+3,5);\n'
        'std::fs::write("installed-profile-artifact.txt", b"INSTALLED_NATIVE_TREE_OK\\n").unwrap();\n'
        'println!("INSTALLED_NATIVE_TREE_OK");\n}\n', encoding="utf-8")
    cargo = Path(manifest["toolchain_roots"][0]) / "bin/cargo.exe"
    script = args.evidence / "installed-start-script.json"
    criteria = ["The provisioned offline Cargo test exits zero",
                "The native test prints INSTALLED_NATIVE_TREE_OK and creates its Windows artifact"]
    def plan(status):
        return tool_turn("task.plan", {"steps": [{
            "id": "installed-cargo", "description": "Verify the freshly installed native profile with offline Cargo",
            "status": status, "acceptance_criteria": criteria,
            "verification_method": "Run the native test through process.run with the selected installed profile"}]})
    write_json(script, [plan("in_progress"),
                        tool_turn("files.read", {"path": crate.name + "/Cargo.toml"}),
                        tool_turn("files.read", {"path": crate.name + "/src/lib.rs"}),
                        tool_turn("process.run", {"argv": [str(cargo), "test", "--offline", "--", "--nocapture"],
                                                 "cwd": crate.name,
                                                 "verification": {"kind": "behavior", "criteria": criteria,
                                                                  "paths": [crate.name + "/Cargo.toml", crate.name + "/src/lib.rs"]}}),
                        plan("completed"),
                        {"deltas": [{"type": "text", "text": "Installed native probe finished."},
                                    {"type": "finish", "reason": "stop"}, {"type": "done"}], "error": None}])
    native = args.evidence / "installed-native.json"
    write_json(native, {"windows_process_tree": {"provisioning": str(args.manifest)}})
    grants = args.evidence / "installed-grants.json"
    write_json(grants, {"mode": "full_access", "workspace_writable": True,
                        "tool_permissions": {"task.plan": "allow", "files.read": "allow", "process.run": "allow"}})
    limits = args.evidence / "installed-process-limits.json"
    write_json(limits, {"timeout_ms": 120000, "output_bytes": 262144,
                        "memory_bytes": 4294967296, "cpu_seconds": 120,
                        "processes": 32, "browser_processes": 512})
    store = args.evidence / "installed-start.sqlite"
    binary = args.installed_bin / "ww.exe"
    command = [str(binary), "--native-sandbox-config", str(native), "--workspace", manifest["workspace"],
               "--grants", str(grants), "--script", str(script), "--harness", "minimal",
               "--process-limits", str(limits), "--store", str(store), "run",
               "Run the explicitly provisioned native offline Cargo acceptance.", "--profile", "coding-basic"]
    result = execute(command, args.evidence, environment, args.evidence / "installed-start.log")
    result["binary_sha256"] = digest(binary)
    result["manifest_sha256"] = digest(args.manifest)
    sessions = []
    try:
        if not store.is_file():
            raise FileNotFoundError("Installed CLI did not create a session store")
        # A refusal before session admission is evidence, not a reason to create
        # an empty SQLite file while inspecting the failed run.
        with contextlib.closing(sqlite3.connect(store.resolve().as_uri() + "?mode=ro", uri=True)) as database:
            sessions = [json.loads(row[0]) for row in database.execute("SELECT data FROM sessions")]
    except (OSError, sqlite3.Error, ValueError, TypeError) as error:
        result["store_error"] = f"{type(error).__name__}: {error}"
    calls = [item for item in objects(sessions) if isinstance(item.get("call"), dict)
             and item["call"].get("name") == "process.run" and "result" in item]
    result["invocations"] = calls
    def successful_call(item):
        tool_result = item.get("result")
        if not isinstance(tool_result, dict):
            return False
        data = tool_result.get("data")
        prepared = item.get("prepared")
        sandbox = prepared.get("sandbox") if isinstance(prepared, dict) else None
        if not isinstance(data, dict) or not isinstance(sandbox, dict):
            return False
        stdout = data.get("stdout")
        return (item.get("status") == "finished"
                and tool_result.get("status") == "succeeded"
                and tool_result.get("error_code") is None
                and tool_result.get("truncated", False) is False
                and all(data.get(flag, False) is False for flag in
                        ("timed_out", "cancelled", "truncated", "stdout_truncated", "stderr_truncated"))
                and data.get("exit_code") == 0
                and isinstance(stdout, str) and "INSTALLED_NATIVE_TREE_OK" in stdout
                and sandbox.get("backend") == "windows-account-job-tree"
                and sandbox.get("account_sid") == manifest["user_sid"]
                and sandbox.get("appcontainer_sid") == manifest["appcontainer_sid"]
                and sandbox.get("appcontainer_capabilities") == []
                and sandbox.get("writer_sid") == manifest["writer_sid"]
                and sandbox.get("provisioning_sha256") == result["manifest_sha256"])
    successful = [item for item in calls if successful_call(item)]
    result["qualified_invocation_count"] = len(successful)
    artifact = crate / "installed-profile-artifact.txt"
    executables = list((crate / "target/debug/deps").glob("*.exe"))
    result["windows_artifacts"] = [{"path": str(path), "sha256": digest(path)} for path in executables]
    result["passed"] = (result["exit_code"] == 0 and "store_error" not in result and len(successful) == 1
                         and artifact.is_file() and artifact.read_bytes() == b"INSTALLED_NATIVE_TREE_OK\n"
                         and bool(executables) and not (args.manifest.parent / "incomplete-run.json").exists())
    write_json(args.evidence / "installed-start-result.json", result)
    print(json.dumps({key: value for key, value in result.items() if key != "invocations"}), flush=True)
    return 0 if result["passed"] else 1


def ordinary_command(arguments):
    """Internal Medium bridge; never accepts an environment or credentials file."""
    parser = argparse.ArgumentParser(description="Internal ordinary native-host command bridge")
    parser.add_argument("task", type=Path)
    parser.add_argument("nonce")
    args = parser.parse_args(arguments)
    task_path = args.task.resolve(strict=True)
    request = json.loads(task_path.read_text(encoding="utf-8"))
    expected_fields = {"nonce", "command", "cwd", "log", "result", "expected_host"}
    if set(request) != expected_fields or request.get("nonce") != args.nonce:
        raise RuntimeError("Invalid ordinary command request fields or nonce")
    if len(args.nonce) != 32 or any(char not in "0123456789abcdef" for char in args.nonce):
        raise RuntimeError("Invalid ordinary command nonce")
    result_path = task_path.parent / f"ordinary-result-{args.nonce}.json"
    if Path(request["result"]) != result_path:
        raise RuntimeError("Ordinary command result must remain beside its nonce-bound request")
    command = request["command"]
    if not isinstance(command, list) or not command or not all(isinstance(item, str) for item in command):
        raise RuntimeError("Ordinary command must be an explicit argument vector")
    executor = WindowsHostExecution()
    current = executor.w.HANDLE()
    executor.require(executor.security.OpenProcessToken(executor.kernel.GetCurrentProcess(), 8,
                                                         executor.ctypes.byref(current)),
                     "Verify the resumed ordinary Python host token")
    try:
        metadata = executor.token_metadata(current)
        executor.verify_medium(metadata, request["expected_host"])
        if metadata["authentication_id"] != request["expected_host"]["authentication_id"]:
            raise RuntimeError("Resumed ordinary host does not match its linked-logon request")
    finally:
        executor.close(current)
    result = {"nonce": args.nonce, "command": command, "cwd": request["cwd"],
              "native_host_token": metadata}
    bridge_exit = 0
    try:
        # This is the existing ordinary subprocess path. It runs only after both
        # the parent and this helper have verified the unmodified Medium token.
        with Path(request["log"]).open("ab") as output:
            completed = subprocess.run(command, cwd=request["cwd"], env=dict(os.environ),
                                       stdin=subprocess.DEVNULL, stdout=output,
                                       stderr=subprocess.STDOUT, check=False)
        result["exit_code"] = completed.returncode
    except (OSError, ValueError) as error:
        result.update({"exit_code": 125, "error": str(error)})
        bridge_exit = 125
    with result_path.open("x", encoding="utf-8") as output:
        json.dump(result, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    return bridge_exit


def main():
    import sys
    if sys.argv[1:2] == ["--ordinary-command"]:
        return ordinary_command(sys.argv[2:])
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=["suite", "installed-start"])
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--installed-bin", type=Path)
    parser.add_argument("--bind-address", help="Explicit assigned non-loopback IPv4 address for the controlled witness")
    parser.add_argument("--diagnostic", choices=["boundaries", "inbound", "resources", "output", "cargo", "console", "ui", "dns", "network-token"])
    args = parser.parse_args()
    args.source = args.source.resolve(strict=True)
    args.manifest = args.manifest.resolve(strict=True)
    if args.installed_bin is not None:
        args.installed_bin = args.installed_bin.resolve(strict=True)
    args.evidence = args.evidence.absolute()
    args.evidence.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ, CARGO_NET_OFFLINE="true")
    return suite(args, environment) if args.phase == "suite" else installed_start(args, environment)


if __name__ == "__main__":
    raise SystemExit(main())
