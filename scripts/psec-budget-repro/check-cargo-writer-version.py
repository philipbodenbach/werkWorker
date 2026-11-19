"""Check the fixed Cargo version workload with and without a write-restricted token."""
from pathlib import Path
import argparse
from datetime import datetime, timezone
import difflib
import hashlib
import json
import os
import re
import runpy
import subprocess
import sys
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("pair", type=Path)
parser.add_argument("new_output", type=Path)
parser.add_argument("--build-only", action="store_true", help="Compile the exact diagnostic configuration, with no payload execution")
parser.add_argument("--private-dirs", action="store_true",
                    help="Use fresh approved workspace and temp children with verified private writer ACLs")
parser.add_argument("--private-desktop", action="store_true",
                    help="Also place each root on a fresh host-owned private desktop with limited writer rights")
parser.add_argument("--match-token-flags", action="store_true",
                    help="Apply the same restricted-token flags to both controls, changing only the restricting SID list")
parser.add_argument("--already-filtered-host", action="store_true",
                    help="On the real filtered user, retain DISABLE_MAX_PRIVILEGE and WRITE_RESTRICTED and verify the final privilege and administrator group limits")
parser.add_argument("--verify-derived-privileges", action="store_true",
                    help="Have PSEC derive privilege removal and reject the held root unless it has only the allowed privilege and no usable administrator group")
parser.add_argument("--remove-extra-privileges", action="store_true",
                    help="Remove extra privileges on the verified suspended private PSEC root and independently reread its complete privilege list")
parser.add_argument("--direct-control", action="store_true",
                    help="Give the control its original host primary token and apply the same privilege removal to the resulting PSEC root")
parser.add_argument("--provided-stations", help="Trial nonce for the two already provisioned administrator-owned USER pairs")
args = parser.parse_args()
if args.provided_stations and (not args.private_desktop or len(args.provided_stations) != 32
                              or any(c not in "0123456789abcdef" for c in args.provided_stations)):
    parser.error("Expected a 32-hex provisioner nonce and private desktop mode")
if args.private_desktop and not args.private_dirs:
    parser.error("Private desktop comparison requires separately bound workspace and scratch")
if args.already_filtered_host and not args.match_token_flags:
    parser.error("This comparison requires identical flags on both arms")
if args.verify_derived_privileges and not args.already_filtered_host:
    parser.error("Derived privilege verification requires the limited host and pre-resume privilege checks")
if args.remove_extra_privileges and not args.verify_derived_privileges:
    parser.error("Privilege removal requires final token verification")
if args.direct_control and not args.remove_extra_privileges:
    parser.error("Direct control requires the same final privilege removal as the writer arm")
pair = args.pair.resolve(strict=True)
out = args.new_output.resolve()
host = runpy.run_path(str(Path(__file__).resolve().parents[1] / "validate-windows-process-tree.py"))["WindowsHostExecution"]()
token = host.current_token_metadata()
host.verify_medium(token, token)
if args.already_filtered_host and token["elevation_type"] != 3:
    raise ValueError("Expected the actual pre-existing limited host token")
out.mkdir(parents=True)
data = (pair / "fixed-cargo.cpp").read_bytes()
paths = (pair / "fixed-paths.inc").read_bytes()
if hashlib.sha256(data).hexdigest() != "76d40dfeb03a883011ba22409d0cd44c7f3b63c400d7bf462af04f4337142688" or hashlib.sha256(paths).hexdigest() != "17a22e9286cafe1d3d865c216fb3f96138e01324d3e2fb462f6388cc18106e57":
    raise ValueError("Retained controller or fixed input paths differ")
before = data.decode().replace("\r\n", "\n")
source = before

def replace(old, new):
    global source
    if source.count(old) != 1:
        raise ValueError("Retained source shape differs")
    source = source.replace(old, new)

replace("void start(bool deny) {", """void start(bool writer) {
        constexpr bool deny = false;
        Handle host_token, restricted;
        require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            &host_token.value), "HostPrimary");
        PSID writer_sid = nullptr;
        require(ConvertStringSidToSidW(L"S-1-5-21-4275439644-3620842777-1714106797-1061", &writer_sid), "PrivateWriter");
        struct SidCleanup { PSID value; ~SidCleanup() { LocalFree(value); } } sid_cleanup{writer_sid};
        SID_AND_ATTRIBUTES restriction{writer_sid, 0};
        if (writer) require(CreateRestrictedToken(host_token.value,
            WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE | LUA_TOKEN, 0, nullptr, 0, nullptr, 1,
            &restriction, &restricted.value), "CreatePrivateWriterToken");""")
replace("const BOOL started = CreateProcessW(image.c_str(), command.data(), nullptr, nullptr, TRUE,",
        "const BOOL started = CreateProcessAsUserW(writer ? restricted.value : host_token.value, image.c_str(), command.data(), nullptr, nullptr, TRUE,")
replace("final_token(process.value,deny);", """final_token(process.value,deny);
        Handle actual;
        require(OpenProcessToken(process.value, TOKEN_QUERY, &actual.value), "ReadCargoToken");
        const auto restrictions = token_info(actual.value, TokenRestrictedSids);
        const auto sids = reinterpret_cast<const TOKEN_GROUPS*>(restrictions.data());
        check(writer ? sids->GroupCount == 1 && EqualSid(sids->Groups[0].Sid, writer_sid) :
            sids->GroupCount == 0, "ActualWriterBeforeResume");
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION one{};
        require(QueryInformationJobObject(job.value, JobObjectBasicAccountingInformation, &one,
            sizeof(one), nullptr), "CountVersionRoot");
        check(one.ActiveProcesses == 1 && one.TotalProcesses == 1, "VersionRootHasSpareJobSlot");
        JOBOBJECT_BASIC_UI_RESTRICTIONS requested_ui{0xff};
        require(SetInformationJobObject(job.value, JobObjectBasicUIRestrictions, &requested_ui,
            sizeof(requested_ui)), "ApplyPrivateJobUi");""")
replace('        std::printf("BEFORE_RESUME', '''        check(ui.UIRestrictionsClass == 0xff, "ActualJobUi");
        std::printf("BEFORE_RESUME''')
replace("for(bool deny : {true,false})", "for(bool writer : {false,true})")
replace('deny ? "A" : "B"', 'writer ? "PSEC_WRITER" : "PSEC_CONTROL"')
replace("one.start(deny);", "one.start(writer);")
if args.match_token_flags:
    replace("if (writer) require(CreateRestrictedToken", "require(CreateRestrictedToken")
    replace("nullptr, 0, nullptr, 1,\n            &restriction", "nullptr, 0, nullptr, writer ? 1 : 0,\n            writer ? &restriction : nullptr")
    if not args.direct_control:
        replace("CreateProcessAsUserW(writer ? restricted.value : host_token.value,", "CreateProcessAsUserW(restricted.value,")
if args.already_filtered_host:
    replace("WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE | LUA_TOKEN",
            "WRITE_RESTRICTED" if args.verify_derived_privileges else "WRITE_RESTRICTED | DISABLE_MAX_PRIVILEGE")
    replace("        const auto restrictions = token_info(actual.value, TokenRestrictedSids);", """        const auto privileges = token_info(actual.value, TokenPrivileges);
        const auto privs = reinterpret_cast<const TOKEN_PRIVILEGES*>(privileges.data());
        LUID traverse{};
        require(LookupPrivilegeValueW(nullptr, L"SeChangeNotifyPrivilege", &traverse), "LookupTraversalPrivilege");
        for (DWORD i = 0; i < privs->PrivilegeCount; ++i) {
            const auto id = privs->Privileges[i].Luid;
            check(id.HighPart == traverse.HighPart && id.LowPart == traverse.LowPart, "OnlyTraversalPrivilege");
        }
        const auto group_info = token_info(actual.value, TokenGroups);
        const auto groups = reinterpret_cast<const TOKEN_GROUPS*>(group_info.data());
        for (DWORD i = 0; i < groups->GroupCount; ++i) {
            const auto& group = groups->Groups[i];
            if (IsWellKnownSid(group.Sid, WinBuiltinAdministratorsSid))
                check((group.Attributes & SE_GROUP_USE_FOR_DENY_ONLY) != 0, "AdministratorsDenyOnly");
        }
        std::printf("FILTER_READBACK privileges=%lu admin_absent_or_deny_only=1\\n", privs->PrivilegeCount);
        const auto restrictions = token_info(actual.value, TokenRestrictedSids);""")
if args.remove_extra_privileges:
    replace('OpenProcessToken(process.value, TOKEN_QUERY, &actual.value)',
            'OpenProcessToken(process.value, TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES, &actual.value)')
    replace("        const auto privileges = token_info(actual.value, TokenPrivileges);", """        const auto previous = token_info(actual.value, TokenPrivileges);
        const auto prior = reinterpret_cast<const TOKEN_PRIVILEGES*>(previous.data());
        LUID retained{};
        require(LookupPrivilegeValueW(nullptr, L"SeChangeNotifyPrivilege", &retained), "ResolveKeptPrivilege");
        for (DWORD i = 0; i < prior->PrivilegeCount; ++i) {
            auto id = prior->Privileges[i].Luid;
            if (id.HighPart == retained.HighPart && id.LowPart == retained.LowPart) continue;
            TOKEN_PRIVILEGES remove{};
            remove.PrivilegeCount = 1;
            remove.Privileges[0] = {id, SE_PRIVILEGE_REMOVED};
            SetLastError(ERROR_SUCCESS);
            require(AdjustTokenPrivileges(actual.value, FALSE, &remove, 0, nullptr, nullptr)
                && GetLastError() == ERROR_SUCCESS, "RemoveExtraRootPrivilege");
            std::printf("REMOVED_PRIVILEGE luid=%lu:%lu\\n", static_cast<DWORD>(id.HighPart), id.LowPart);
        }
        const auto privileges = token_info(actual.value, TokenPrivileges);""")
replace("complete = complete && wait==WAIT_OBJECT_0;", """complete = complete && wait==WAIT_OBJECT_0 && code==0 &&
                one.captured.find("cargo 1.98.1 (797e8a9bc 2026-08-05)") != std::string::npos;""")
if args.private_dirs:
    include = Path(__file__).with_name("private_writer_dirs.inc").read_bytes()
    (out / "private_writer_dirs.inc").write_bytes(include)
    nonce = uuid.uuid4().hex
    replace("struct Invocation {", '#include "private_writer_dirs.inc"\nstruct Invocation {\n    WriterDirectory private_work, private_temp;')
    replace('        const auto spec_path=specpath.c_str(), workspace=work.c_str(), scratch=temp.c_str();',
            f'''        const auto user_info = token_info(host_token.value, TokenUser);
        const auto user_sid = sid_text(reinterpret_cast<const TOKEN_USER*>(user_info.data())->User.Sid);
        const std::wstring leaf = L"\\\\ww-writer-{nonce}-" + std::wstring(writer ? L"writer" : L"control");
        private_work.create(work + leaf, user_sid);
        private_temp.create(temp + leaf, user_sid);
        const auto spec_path=specpath.c_str(), workspace=private_work.path.c_str(), scratch=private_temp.path.c_str();''')
    replace("        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION one{};", """        const auto package_info = token_info(actual.value, TokenAppContainerSid);
        const auto package = sid_text(reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package_info.data())->TokenAppContainer);
        private_work.bind(user_sid, package);
        private_temp.bind(user_sid, package);
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION one{};""")
    if args.private_desktop:
        (out / "private_writer_desktop.inc").write_bytes(Path(__file__).with_name("private_writer_desktop.inc").read_bytes())
        replace('struct Invocation {\n    WriterDirectory', '#include "private_writer_desktop.inc"\nstruct Invocation {\n    WriterDesktop user_objects;\n    WriterDirectory')
        objects = (f'user_objects.open_provisioned(L"{args.provided_stations[:31]}" + std::wstring(writer ? L"1" : L"0"), user_sid);'
                   if args.provided_stations else
                   f'user_objects.create(L"{nonce}-" + std::wstring(writer ? L"writer" : L"control"), user_sid);')
        replace("        private_work.create(work + leaf, user_sid);", "        " + objects + "\n        private_work.create(work + leaf, user_sid);")
        replace("        startup.lpAttributeList = attributes;", r'''        auto desktop = user_objects.station_name + L"\\" + user_objects.desktop_name;
        startup.StartupInfo.lpDesktop = desktop.data();
        startup.lpAttributeList = attributes;''')
        replace("        private_work.bind(user_sid, package);", "        user_objects.bind(package);\n        private_work.bind(user_sid, package);")
patch = "".join(difflib.unified_diff(before.splitlines(keepends=True), source.splitlines(keepends=True),
                                     fromfile="a/fixed-cargo.cpp", tofile="b/fixed-cargo.cpp"))
(out / "writer-version.patch").write_bytes(patch.encode())
(out / "fixed-cargo.cpp").write_bytes(source.encode())
(out / "fixed-paths.inc").write_bytes(paths)
vswhere = Path(os.environ["ProgramFiles(x86)"]) / "Microsoft Visual Studio/Installer/vswhere.exe"
vs = subprocess.check_output([str(vswhere), "-latest", "-products", "*", "-requires",
                              "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"],
                             text=True).strip()
vcvars = Path(vs) / "VC/Auxiliary/Build/vcvars64.bat"
if not vs or not vcvars.is_file():
    raise OSError("x64 MSVC developer environment missing")
compile_cmd = '@echo off\ncall "' + str(vcvars) + '"\nif errorlevel 1 exit /b %errorlevel%\ncl.exe /nologo /Bv /W4 /WX /EHsc /MT /O2 /std:c++17 /Fe:version.exe fixed-cargo.cpp\nexit /b %errorlevel%\n'
(out / "compile.cmd").write_text(compile_cmd, encoding="ascii")
receipt = {"source_pair": str(pair), "host_token": token, "utc": datetime.now(timezone.utc).isoformat(),
           "workload": "pinned cargo.exe --version", "private_directories": args.private_dirs,
           "private_desktop": args.private_desktop, "matched_restricted_token_flags": args.match_token_flags,
           "already_filtered_host": args.already_filtered_host,
           "require_psec_to_derive_limited_privileges": args.verify_derived_privileges,
           "remove_extra_suspended_root_privileges": args.remove_extra_privileges,
           "use_original_primary_for_control": args.direct_control,
           "provisioned_user_objects": bool(args.provided_stations),
           "product": False, "commands": []}
commands = [[str(Path(os.environ["SystemRoot"]) / "System32/cmd.exe"), "/d", "/c", "compile.cmd"],
            [str(out / "version.exe")]]
if args.build_only:
    commands = commands[:1]
receipt["workloads"] = {"PSEC_CONTROL": {"status": "NOT_RUN"}, "PSEC_WRITER": {"status": "NOT_RUN"}}
for command, name in zip(commands, ("compile", "version")):
    log = out / (name + ".log")
    with log.open("wb") as stream:
        result = subprocess.run(command, cwd=out, stdout=stream, stderr=subprocess.STDOUT)
    receipt["commands"].append({"argv": command, "exit": result.returncode, "log": log.name})
    if result.returncode:
        break
if len(receipt["commands"]) == 2:
    raw = (out / "version.log").read_text(errors="replace")
    for name, detail in re.findall(r"^ARM (PSEC_CONTROL|PSEC_WRITER)\n(.*?)(?=^ARM |\Z)", raw, re.M | re.S):
        status = re.search(r"^WORKLOAD_EXIT (\d+)$", detail, re.M)
        if status:
            code = int(status.group(1))
            receipt["workloads"][name] = {
                "exit": code, "status": "PASS" if code == 0 and
                "cargo 1.98.1 (797e8a9bc 2026-08-05)" in detail.splitlines() else "FAIL",
                "confirmed_cleanup": "CLEANUP active=0 confirmed=1" in detail.splitlines(),
            }
    if args.private_dirs:
        receipt["all_four_private_directories_closed"] = raw.count("PRIVATE_DIRECTORY_CLOSE removed=1") == 4
receipt["files_sha256"] = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in out.iterdir() if p.is_file()}
(out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
print(json.dumps(receipt["commands"]))
sys.exit(result.returncode or int(not receipt.get("all_four_private_directories_closed", True)))
