// Read the public child-process mitigation on a never-resumed cargo.exe.
// Both cases use DETACHED_PROCESS, no inherited handles and the same Job limits.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <map>
#include <vector>

using CreatePsec = HRESULT(WINAPI*)(const void*, DWORD, DWORD, HANDLE*);
using ClosePsec = void(WINAPI*)(HANDLE);

enum class JobMode { outer, list, assign_suspended };
constexpr DWORD worker_ui = JOB_OBJECT_UILIMIT_HANDLES | JOB_OBJECT_UILIMIT_READCLIPBOARD
    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS | JOB_OBJECT_UILIMIT_GLOBALATOMS
    | JOB_OBJECT_UILIMIT_DESKTOP | JOB_OBJECT_UILIMIT_EXITWINDOWS;

static bool run(const wchar_t* cargo, HANDLE psec, JobMode mode) {
    const bool leaf_budget = mode != JobMode::outer;
    const DWORD count = 1 + (psec ? 1 : 0) + (mode == JobMode::list ? 1 : 0);
    SIZE_T size = 0;
    InitializeProcThreadAttributeList(nullptr, count, 0, &size);
    if (!size) return false;
    std::vector<ULONG_PTR> storage((size + sizeof(ULONG_PTR) - 1) / sizeof(ULONG_PTR));
    auto attrs = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    if (!InitializeProcThreadAttributeList(attrs, count, 0, &size)) return false;
    DWORD child = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limits.BasicLimitInformation.ActiveProcessLimit = 2;
    HANDLE job = CreateJobObjectW(nullptr, nullptr);
    HANDLE leaf = leaf_budget ? CreateJobObjectW(nullptr, nullptr) : nullptr;
    HANDLE hierarchy[] = {job, leaf};
    PROCESS_INFORMATION pi{};
    STARTUPINFOEXW si{};
    si.StartupInfo.cb = sizeof(si);
    si.lpAttributeList = attrs;
    std::wstring args = L"\"" + std::wstring(cargo) + L"\" --version";
    const auto stage = std::filesystem::path(cargo).parent_path().parent_path();
    const auto cwd = stage / L"workspace";
    const auto temp = (stage / L"scratch").wstring();
    wchar_t windows[MAX_PATH]{}, system[MAX_PATH]{};
    if (!GetWindowsDirectoryW(windows, MAX_PATH) || !GetSystemDirectoryW(system, MAX_PATH)) return false;
    const std::map<std::wstring, std::wstring> variables{
        {L"HOME",temp}, {L"LOCALAPPDATA",temp}, {L"PATH",system}, {L"SystemRoot",windows},
        {L"TEMP",temp}, {L"TMP",temp}, {L"USERPROFILE",temp}, {L"WINDIR",windows}};
    std::vector<wchar_t> block;
    for (const auto& var : variables) {
        const auto line = var.first + L"=" + var.second;
        block.insert(block.end(), line.begin(), line.end());
        block.push_back(0);
    }
    block.push_back(0);
    bool ok = job && SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits, sizeof(limits))
        && UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
                                      &child, sizeof(child), nullptr, nullptr)
        && (!psec || UpdateProcThreadAttribute(attrs, 0, 0x20023, &psec, sizeof(psec), nullptr, nullptr));
    if (leaf_budget && ok) {
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        ok = leaf && SetInformationJobObject(leaf, JobObjectExtendedLimitInformation, &limits, sizeof(limits));
        if (ok && mode == JobMode::list)
            ok = UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                                           hierarchy, sizeof(hierarchy), nullptr, nullptr) != FALSE;
    }
    if (ok) ok = CreateProcessW(cargo, args.data(), nullptr, nullptr, FALSE,
        CREATE_SUSPENDED | DETACHED_PROCESS | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
        block.data(), cwd.c_str(), &si.StartupInfo, &pi) != FALSE;
    const DWORD create_error = ok ? 0 : GetLastError();
    BOOL member = FALSE;
    PROCESS_MITIGATION_CHILD_PROCESS_POLICY effective{};
    PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY win32k{};
    bool read = ok && (mode == JobMode::list || AssignProcessToJobObject(job, pi.hProcess))
        && (mode != JobMode::assign_suspended || AssignProcessToJobObject(leaf, pi.hProcess))
        && IsProcessInJob(pi.hProcess, job, &member)
        && GetProcessMitigationPolicy(pi.hProcess, ProcessChildProcessPolicy, &effective, sizeof(effective))
        && GetProcessMitigationPolicy(pi.hProcess, ProcessSystemCallDisablePolicy, &win32k, sizeof(win32k));
    const DWORD read_error = read ? 0 : GetLastError();
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION outer{};
    const bool outer_read = job && QueryInformationJobObject(job, JobObjectExtendedLimitInformation,
        &outer, sizeof(outer), nullptr);
    const auto& outer_limits = outer.BasicLimitInformation;
    const bool outer_valid = outer_read && outer_limits.ActiveProcessLimit == 2
        && (outer_limits.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS)
        && !(outer_limits.LimitFlags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK));
    std::printf("psec=%d outer_read=%d outer_limit=%lu outer_flags=0x%lx\n", psec != nullptr,
                outer_read, outer_limits.ActiveProcessLimit, outer_limits.LimitFlags);
    BOOL in_leaf = FALSE;
    bool leaf_valid = false;
    if (leaf_budget && read) {
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION actual{};
        JOBOBJECT_BASIC_UI_RESTRICTIONS ui{worker_ui};
        const bool membership_read = IsProcessInJob(pi.hProcess, leaf, &in_leaf) != FALSE;
        const bool limits_read = QueryInformationJobObject(leaf, JobObjectExtendedLimitInformation,
            &actual, sizeof(actual), nullptr) != FALSE;
        // Apply the required outer-job UI policy only after the hierarchy exists.
        const bool ui_set = SetInformationJobObject(job, JobObjectBasicUIRestrictions, &ui, sizeof(ui)) != FALSE;
        const DWORD ui_error = ui_set ? 0 : GetLastError();
        ui.UIRestrictionsClass = 0;
        const bool ui_read = QueryInformationJobObject(job, JobObjectBasicUIRestrictions,
            &ui, sizeof(ui), nullptr) != FALSE;
        const auto& basic = actual.BasicLimitInformation;
        leaf_valid = member && membership_read && in_leaf && limits_read
            && basic.ActiveProcessLimit == 1
            && (basic.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS)
            && !(basic.LimitFlags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK))
            && ui_set && ui_read && ui.UIRestrictionsClass == worker_ui;
        std::printf("psec=%d leaf_member=%d leaf_limit=%lu leaf_flags=0x%lx outer_ui_set=%d "
                    "ui_error=%lu outer_ui=0x%lx\n", psec != nullptr, in_leaf, basic.ActiveProcessLimit,
                    basic.LimitFlags, ui_set, ui_error, ui.UIRestrictionsClass);
    }
    std::printf("psec=%d pid=%lu create_error=%lu read=%d read_error=%lu member=%d "
                "child_flags=0x%lx win32k_flags=0x%lx resumed=0\n",
                psec != nullptr, pi.dwProcessId, create_error, read, read_error, member, effective.Flags, win32k.Flags);
    bool clean = true;
    if (pi.hProcess) {
        clean = TerminateProcess(pi.hProcess, 91) && WaitForSingleObject(pi.hProcess, 10000) == WAIT_OBJECT_0;
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting{};
    bool drained = job && QueryInformationJobObject(job, JobObjectBasicAccountingInformation,
        &accounting, sizeof(accounting), nullptr) && accounting.ActiveProcesses == 0;
    std::printf("reaped=%d empty=%d total=%lu\n", clean, drained, accounting.TotalProcesses);
    if (leaf) {
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION leaf_accounting{};
        drained = drained && QueryInformationJobObject(leaf, JobObjectBasicAccountingInformation,
            &leaf_accounting, sizeof(leaf_accounting), nullptr) && leaf_accounting.ActiveProcesses == 0;
        std::printf("both_empty=%d leaf_total=%lu\n", drained, leaf_accounting.TotalProcesses);
        CloseHandle(leaf);
    }
    if (job) CloseHandle(job);
    DeleteProcThreadAttributeList(attrs);
    const bool budget = leaf_budget ? leaf_valid : effective.NoChildProcessCreation && !effective.AllowSecureProcessCreation;
    return clean && drained && read && member && outer_valid && budget && !win32k.DisallowWin32kSystemCalls;
}

int wmain(int argc, wchar_t** argv) {
    if (argc < 3 || argc > 4 || std::filesystem::path(argv[1]).filename() != L"cargo.exe") return 2;
    JobMode mode = JobMode::outer;
    if (argc == 4) {
        if (wcscmp(argv[3], L"leaf-budget") == 0) mode = JobMode::list;
        else if (wcscmp(argv[3], L"assign-suspended") == 0) mode = JobMode::assign_suspended;
        else return 2;
    }
    std::ifstream policy(std::filesystem::path(argv[2]), std::ios::binary);
    std::vector<char> bytes((std::istreambuf_iterator<char>(policy)), {});
    if (bytes.empty() || bytes.size() > 65536) return 2;
    HMODULE module = LoadLibraryExW(L"processmodel.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!module) return 2;
    auto create = reinterpret_cast<CreatePsec>(GetProcAddress(module, "CreateProcessSecurityEnvironment"));
    auto close = reinterpret_cast<ClosePsec>(GetProcAddress(module, "CloseProcessSecurityEnvironment"));
    HANDLE env = nullptr;
    if (!create || !close || create(bytes.data(), static_cast<DWORD>(bytes.size()), 1, &env) != S_OK || !env) return 2;
    bool baseline = run(argv[1], nullptr, mode);
    bool candidate = run(argv[1], env, mode);
    close(env);
    FreeLibrary(module);
    return !baseline ? 2 : candidate ? 0 : 1;
}
