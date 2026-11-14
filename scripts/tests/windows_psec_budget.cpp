// Diagnostic only. No product backend, arbitrary payload, or fallback execution.
// Missing atomic Job assignment or an exceeded configured budget returns exit 1.
// Explicit suspended assignment is a separate diagnostic, never a runtime fallback.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <sddl.h>
#include <cstdio>
#include <fstream>
#include <map>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>
#pragma comment(lib, "advapi32.lib")

struct Handle {
    HANDLE value = nullptr;
    Handle() = default;
    explicit Handle(HANDLE handle) : value(handle) {}
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
    Handle(Handle&& other) noexcept : value(other.value) { other.value = nullptr; }
    void close() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); value = nullptr; }
    ~Handle() { close(); }
};
static void require(bool value, const char* operation) {
    if (!value) {
        std::printf("ERROR operation=%s win32=%lu\n", operation, GetLastError());
        throw std::runtime_error(operation);
    }
}
static void check(bool value, const char* condition) {
    if (!value) {
        std::printf("ASSERTION_FAILED condition=%s\n", condition);
        throw std::runtime_error(condition);
    }
}
static std::wstring image_path() {
    wchar_t path[32768]{};
    require(GetModuleFileNameW(nullptr, path, 32768) != 0, "OwnImagePath");
    return path;
}
static std::wstring system_path() {
    wchar_t path[32768]{};
    require(GetSystemDirectoryW(path, 32768) != 0, "SystemDirectory");
    return path;
}
static std::wstring quote(const std::wstring& value) { return L"\"" + value + L"\""; }

using CreatePsec = HRESULT(WINAPI*)(const void*, DWORD, DWORD, HANDLE*);
using ClosePsec = void(WINAPI*)(HANDLE);
struct PsecApi {
    HMODULE module = nullptr;
    CreatePsec create = nullptr;
    ClosePsec close = nullptr;
    PsecApi() {
        module = LoadLibraryExW(L"processmodel.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
        require(module != nullptr, "LoadSystem32ProcessModel");
        create = reinterpret_cast<CreatePsec>(GetProcAddress(module, "CreateProcessSecurityEnvironment"));
        close = reinterpret_cast<ClosePsec>(GetProcAddress(module, "CloseProcessSecurityEnvironment"));
        require(create && close, "PsecExports");
    }
    ~PsecApi() { if (module) FreeLibrary(module); }
};

static void token_receipt(HANDLE process, DWORD pid) {
    Handle token;
    require(OpenProcessToken(process, TOKEN_QUERY, &token.value) != FALSE, "TokenQueryOnly");
    DWORD app = 0, elevated = 0, size = 0;
    require(GetTokenInformation(token.value, TokenIsAppContainer, &app, sizeof(app), &size) != FALSE, "AppContainerFlag");
    require(GetTokenInformation(token.value, TokenElevation, &elevated, sizeof(elevated), &size) != FALSE, "ElevationFlag");
    GetTokenInformation(token.value, TokenAppContainerSid, nullptr, 0, &size);
    std::vector<BYTE> data(size);
    require(size && GetTokenInformation(token.value, TokenAppContainerSid, data.data(), size, &size), "PackageSid");
    const auto package = reinterpret_cast<TOKEN_APPCONTAINER_INFORMATION*>(data.data())->TokenAppContainer;
    LPWSTR text = nullptr;
    if (package) require(ConvertSidToStringSidW(package, &text) != FALSE, "SidText");
    std::printf("TOKEN pid=%lu appcontainer=%lu elevated=%lu package=%ls\n", pid, app, elevated, text ? text : L"none");
    if (text) LocalFree(text);
    check(app == 1 && elevated == 0, "ExpectedUnprivilegedContainer");
}

static DWORD conhost_open(DWORD access) {
    Handle file(CreateFileW((system_path() + L"\\conhost.exe").c_str(), access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr, OPEN_EXISTING, 0, nullptr));
    return file.value == INVALID_HANDLE_VALUE ? GetLastError() : ERROR_SUCCESS;
}
static int payload(bool child, bool deny_check) {
    token_receipt(GetCurrentProcess(), GetCurrentProcessId());
    PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY mitigation{};
    require(GetProcessMitigationPolicy(GetCurrentProcess(), ProcessSystemCallDisablePolicy,
        &mitigation, sizeof(mitigation)) != FALSE && mitigation.Flags == 1, "Win32kDisabled");
    if (deny_check) {
        const DWORD read = conhost_open(GENERIC_READ), execute = conhost_open(FILE_EXECUTE);
        std::printf("FILE_DENY read_error=%lu execute_error=%lu\n", read, execute);
        check(read == ERROR_ACCESS_DENIED && execute == ERROR_ACCESS_DENIED, "ExplicitConhostDeny");
    }
    if (child) {
        auto image = image_path();
        auto command = quote(image) + L" --payload 0 0";
        STARTUPINFOW startup{};
        startup.cb = sizeof(startup);
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
        startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
        startup.hStdError = GetStdHandle(STD_ERROR_HANDLE);
        PROCESS_INFORMATION info{};
        const BOOL started = CreateProcessW(image.c_str(), command.data(), nullptr, nullptr, TRUE,
            CREATE_NO_WINDOW, nullptr, nullptr, &startup, &info);
        const DWORD start_error = started ? ERROR_SUCCESS : GetLastError();
        std::printf("CHILD_CREATE creator=%lu child=%lu flags=CREATE_NO_WINDOW success=%d error=%lu\n",
            GetCurrentProcessId(), info.dwProcessId, started, start_error);
        check(started != FALSE, "OrdinaryNoWindowChild");
        Handle process(info.hProcess), thread(info.hThread);
        require(WaitForSingleObject(process.value, 15000) == WAIT_OBJECT_0, "ChildExit");
        DWORD code = 999;
        require(GetExitCodeProcess(process.value, &code) != FALSE && code == 0, "ChildExitCode");
    } else {
        std::printf("READY pid=%lu\n", GetCurrentProcessId());
        char command = 0;
        DWORD received = 0;
        require(ReadFile(GetStdHandle(STD_INPUT_HANDLE), &command, 1, &received, nullptr) != FALSE
            && received == 1 && command == 'G', "HostReleaseByte");
    }
    return 0;
}

struct Invocation {
    PsecApi& api;
    HANDLE security = nullptr;
    Handle job, process, thread, output, input;
    std::vector<Handle> observed;
    std::string captured;
    bool cleaned = false;
    explicit Invocation(PsecApi& value) : api(value) {}
    ~Invocation() { if (!cleaned) cleanup(); }

    void drain() {
        for (;;) {
            DWORD available = 0;
            if (!PeekNamedPipe(output.value, nullptr, 0, nullptr, &available, nullptr)) {
                require(GetLastError() == ERROR_BROKEN_PIPE, "PeekPrivateOutput");
                return;
            }
            if (!available) return;
            char bytes[2048]; DWORD read = 0;
            const DWORD requested = available < sizeof(bytes) ? available : static_cast<DWORD>(sizeof(bytes));
            require(ReadFile(output.value, bytes, requested, &read, nullptr) != FALSE, "ReadAvailableOutput");
            captured.append(bytes, read);
            require(captured.size() <= 65536, "BoundedFixtureOutput");
        }
    }
    bool cleanup() noexcept {
        bool ok = true;
        if (job.value && !TerminateJobObject(job.value, 91)) {
            const DWORD error = GetLastError();
            std::printf("CLEANUP_ERROR operation=TerminateJob win32=%lu\n", error); ok = false;
        }
        if (security) { api.close(security); security = nullptr; }
        if (process.value) {
            const DWORD wait = WaitForSingleObject(process.value, 5000);
            const DWORD error = wait == WAIT_FAILED ? GetLastError() : ERROR_SUCCESS;
            std::printf("CLEANUP_ROOT wait=%lu win32=%lu\n", wait, error);
            ok = wait == WAIT_OBJECT_0 && ok;
            if (wait != WAIT_OBJECT_0) {
                // The unrestricted atomic-assignment control might never have
                // joined the Job. Reclaim that exact suspended process through
                // its creation handle; a failed containment cleanup stays failed.
                const BOOL terminated = TerminateProcess(process.value, 91);
                const DWORD terminate_error = terminated ? ERROR_SUCCESS : GetLastError();
                const DWORD final_wait = WaitForSingleObject(process.value, 5000);
                std::printf("CLEANUP_FALLBACK retained_handle=1 terminated=%d win32=%lu wait=%lu qualification_failed=1\n",
                    terminated, terminate_error, final_wait);
            }
        }
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION counts{};
        if (job.value) {
            for (unsigned attempt = 0; attempt < 1000; ++attempt) {
                if (!QueryInformationJobObject(job.value, JobObjectBasicAccountingInformation, &counts, sizeof(counts), nullptr)) {
                    const DWORD error = GetLastError();
                    std::printf("CLEANUP_ERROR operation=QueryOwnJob win32=%lu\n", error); ok = false; break;
                }
                if (!counts.ActiveProcesses) break;
                Sleep(5);
            }
            ok = ok && counts.ActiveProcesses == 0;
        }
        for (const auto& handle : observed) {
            const DWORD wait = WaitForSingleObject(handle.value, 5000);
            const DWORD error = wait == WAIT_FAILED ? GetLastError() : ERROR_SUCCESS;
            if (wait != WAIT_OBJECT_0) std::printf("CLEANUP_ERROR operation=WaitOwnedProcess wait=%lu win32=%lu\n", wait, error);
            ok = (wait == WAIT_OBJECT_0) && ok;
        }
        std::printf("CLEANUP active=%lu confirmed=%d\n", counts.ActiveProcesses, ok);
        cleaned = true;
        return ok;
    }
    bool start(const wchar_t* spec_path, const wchar_t* workspace, const wchar_t* scratch,
               DWORD budget, DWORD flags, bool child, bool deny_check,
               bool atomic_assignment, bool use_psec = true) {
        std::ifstream source(spec_path, std::ios::binary);
        std::vector<char> spec((std::istreambuf_iterator<char>(source)), {});
        check(!spec.empty() && spec.size() < 1024 * 1024, "ReadSdkEncodedSpec");
        if (use_psec) {
            const HRESULT hr = api.create(spec.data(), static_cast<DWORD>(spec.size()), 1, &security);
            std::printf("PSEC_CREATE hresult=0x%08lx terminate_on_close=1 handle=%d\n", static_cast<DWORD>(hr), security != nullptr);
            check(hr == S_OK && security, "CreateNativePsecNoFallback");
        }
        job.value = CreateJobObjectW(nullptr, nullptr);
        require(job.value != nullptr, "CreateOwnedJob");
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_JOB_TIME;
        limits.BasicLimitInformation.ActiveProcessLimit = budget;
        limits.BasicLimitInformation.PerJobUserTimeLimit.QuadPart = 30LL * 10000000;
        limits.JobMemoryLimit = 512 * 1024 * 1024;
        require(SetInformationJobObject(job.value, JobObjectExtendedLimitInformation, &limits, sizeof(limits)) != FALSE, "HardJobLimits");

        SECURITY_ATTRIBUTES inheritable{sizeof(inheritable), nullptr, TRUE};
        Handle child_input, child_output;
        require(CreatePipe(&output.value, &child_output.value, &inheritable, 0) != FALSE, "OutputPipe");
        require(CreatePipe(&child_input.value, &input.value, &inheritable, 0) != FALSE, "InputPipe");
        require(SetHandleInformation(output.value, HANDLE_FLAG_INHERIT, 0) != FALSE
            && SetHandleInformation(input.value, HANDLE_FLAG_INHERIT, 0) != FALSE, "PrivateHostPipeEnds");
        SIZE_T size = 0;
        const DWORD attribute_count = 1 + (use_psec ? 1 : 0) + (atomic_assignment ? 1 : 0);
        InitializeProcThreadAttributeList(nullptr, attribute_count, 0, &size);
        std::vector<ULONG_PTR> storage((size + sizeof(ULONG_PTR) - 1) / sizeof(ULONG_PTR));
        auto attributes = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
        require(InitializeProcThreadAttributeList(attributes, attribute_count, 0, &size) != FALSE, "InitializeAttributes");
        struct AttributeCleanup { LPPROC_THREAD_ATTRIBUTE_LIST value; ~AttributeCleanup() { DeleteProcThreadAttributeList(value); } } attribute_cleanup{attributes};
        HANDLE inherited[] = {child_input.value, child_output.value};
        if (use_psec) require(UpdateProcThreadAttribute(attributes, 0, 0x20023, &security, sizeof(security), nullptr, nullptr) != FALSE, "PsecAttribute");
        require(UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited, sizeof(inherited), nullptr, nullptr) != FALSE, "StdioOnlyHandleList");
        if (atomic_assignment) require(UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST, &job.value, sizeof(job.value), nullptr, nullptr) != FALSE, "AtomicJobAssignment");
        STARTUPINFOEXW startup{};
        startup.StartupInfo.cb = sizeof(startup);
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = child_input.value;
        startup.StartupInfo.hStdOutput = child_output.value;
        startup.StartupInfo.hStdError = child_output.value;
        startup.lpAttributeList = attributes;
        wchar_t windows[32768]{};
        require(GetWindowsDirectoryW(windows, 32768) != 0, "WindowsDirectory");
        const std::map<std::wstring, std::wstring> values{{L"HOME", scratch}, {L"LOCALAPPDATA", scratch},
            {L"PATH", system_path()}, {L"SystemRoot", windows}, {L"TEMP", scratch}, {L"TMP", scratch},
            {L"USERPROFILE", scratch}, {L"WINDIR", windows}};
        std::vector<wchar_t> environment;
        for (const auto& entry : values) {
            auto value = entry.first + L"=" + entry.second;
            environment.insert(environment.end(), value.begin(), value.end()); environment.push_back(0);
        }
        environment.push_back(0);
        auto image = image_path();
        auto command = quote(image) + L" --payload " + (child ? L"1" : L"0") + L" " + (deny_check ? L"1" : L"0");
        PROCESS_INFORMATION info{};
        const BOOL started = CreateProcessW(image.c_str(), command.data(), nullptr, nullptr, TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED | flags,
            environment.data(), workspace, &startup.StartupInfo, &info);
        const DWORD start_error = started ? ERROR_SUCCESS : GetLastError();
        std::printf("ROOT_CREATE pid=%lu flags=0x%lx budget=%lu success=%d error=%lu\n", info.dwProcessId, flags, budget, started, start_error);
        check(started != FALSE, "CreateSuspendedRoot");
        process.value = info.hProcess; thread.value = info.hThread;
        require(SuspendThread(thread.value) == 1 && ResumeThread(thread.value) == 2, "ActualInitialSuspendCount");
        if (!atomic_assignment) require(AssignProcessToJobObject(job.value, process.value) != FALSE, "DiagnosticAssignWhileSuspended");
        BOOL member = FALSE;
        require(IsProcessInJob(process.value, job.value, &member) != FALSE, "QueryOwnJobBeforeResume");
        std::printf("ROOT_JOB psec=%d atomic_assignment=%d query_success=1 member=%d suspended=1\n", use_psec, atomic_assignment, member);
        // No payload thread is resumed when the creation attribute was ignored.
        if (!member && atomic_assignment && use_psec) return false;
        check(member != FALSE, "OwnJobBeforeResume");
        DWORD job_flags = 0;
        require(GetHandleInformation(job.value, &job_flags) != FALSE && !(job_flags & HANDLE_FLAG_INHERIT), "NonInheritedJobHandle");
        std::printf("ROOT_CONTAINMENT atomic_job=%d member=1 suspended=1 stdio_handle_list=1 job_inherited=0\n", atomic_assignment);
        child_input.close(); child_output.close();
        // The unrestricted control only checks assignment; it never runs payload.
        if (use_psec) require(ResumeThread(thread.value) == 1, "ResumeAssignedRoot");
        return true;
    }
    bool observe(DWORD budget) {
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION counts{};
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
        std::vector<ULONG_PTR> storage(1026);
        auto ids = reinterpret_cast<JOBOBJECT_BASIC_PROCESS_ID_LIST*>(storage.data());
        require(QueryInformationJobObject(job.value, JobObjectBasicAccountingInformation, &counts, sizeof(counts), nullptr) != FALSE, "OwnedJobCount");
        require(QueryInformationJobObject(job.value, JobObjectExtendedLimitInformation, &limits, sizeof(limits), nullptr) != FALSE, "OwnedJobLimits");
        require(QueryInformationJobObject(job.value, JobObjectBasicProcessIdList, ids,
            static_cast<DWORD>(storage.size() * sizeof(ULONG_PTR)), nullptr) != FALSE, "OwnedJobPids");
        check(ids->NumberOfAssignedProcesses == ids->NumberOfProcessIdsInList, "CompleteOwnedPidList");
        const DWORD required_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_JOB_TIME;
        check(limits.BasicLimitInformation.ActiveProcessLimit == budget
            && (limits.BasicLimitInformation.LimitFlags & required_flags) == required_flags
            && !(limits.BasicLimitInformation.LimitFlags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK))
            && limits.JobMemoryLimit == 512 * 1024 * 1024
            && limits.BasicLimitInformation.PerJobUserTimeLimit.QuadPart == 30LL * 10000000,
            "ConfiguredLimitsStillActiveNoBreakaway");
        std::printf("JOB configured=%lu active=%lu flags=0x%lx\n", budget, counts.ActiveProcesses, limits.BasicLimitInformation.LimitFlags);
        for (DWORD index = 0; index < ids->NumberOfProcessIdsInList; ++index) {
            const DWORD pid = static_cast<DWORD>(ids->ProcessIdList[index]);
            Handle handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid));
            require(handle.value != nullptr, "PinOwnedProcessQueryOnly");
            BOOL member = FALSE;
            require(IsProcessInJob(handle.value, job.value, &member) != FALSE && member, "RevalidateJobBeforeMetadata");
            require(WaitForSingleObject(handle.value, 0) == WAIT_TIMEOUT, "ObservedProcessAlive");
            wchar_t path[32768]{}; DWORD length = 32768;
            require(QueryFullProcessImageNameW(handle.value, 0, path, &length) != FALSE, "OwnedProcessImage");
            std::printf("OWNED_PROCESS pid=%lu member=1 access=0x101000 image=%ls\n", pid, path);
            token_receipt(handle.value, pid);
            observed.emplace_back(std::move(handle));
        }
        return counts.ActiveProcesses > budget;
    }
};

int wmain(int argc, wchar_t** argv) {
    setvbuf(stdout, nullptr, _IONBF, 0);
    try {
        if (argc == 4 && wcscmp(argv[1], L"--payload") == 0) return payload(wcscmp(argv[2], L"1") == 0, wcscmp(argv[3], L"1") == 0);
        if (argc != 5) { std::puts("usage: windows-psec-budget.exe baseline.bin conhost-denied.bin workspace scratch"); return 2; }
        const DWORD read = conhost_open(GENERIC_READ), execute = conhost_open(FILE_EXECUTE);
        std::printf("HOST_CONHOST_OPEN read_error=%lu execute_error=%lu\n", read, execute);
        check(read == 0 && execute == 0, "HostConhostOpenPositiveControl");
        PsecApi api;
        {
            Invocation control(api);
            check(control.start(argv[1], argv[3], argv[4], 1, DETACHED_PROCESS, false, false, true, false), "AtomicJobPositiveControl");
            check(control.cleanup(), "AtomicJobControlCleanup");
            std::puts("ATOMIC_JOB_CONTROL passed=1 payload_resumed=0");
        }
        struct Case { const char* name; DWORD budget; DWORD flags; bool child; bool deny; bool atomic; };
        const Case cases[] = {
            {"psec-atomic-job", 1, DETACHED_PROCESS, false, false, true},
            {"detached-initial-control", 1, DETACHED_PROCESS, false, false, false},
            {"headless-initial", 1, CREATE_NO_WINDOW, false, false, false},
            {"detached-root-headless-child", 2, DETACHED_PROCESS, true, false, false},
            {"conhost-denied-headless-child", 2, DETACHED_PROCESS, true, true, false}
        };
        bool exceeded = false, incomplete = false, assignment_ignored = false;
        for (const auto& test : cases) {
            std::printf("CASE name=%s\n", test.name);
            Invocation invocation(api);
            try {
                const bool assigned = invocation.start(argv[test.deny ? 2 : 1], argv[3], argv[4], test.budget, test.flags, test.child, test.deny, test.atomic);
                if (!assigned) {
                    assignment_ignored = true;
                    std::puts("ATOMIC_ASSIGNMENT_IGNORED observed=1 payload_resumed=0");
                } else {
                    const auto deadline = GetTickCount64() + 10000;
                    bool ready = false;
                    while (GetTickCount64() < deadline) {
                        invocation.drain();
                        if (invocation.captured.find("READY pid=") != std::string::npos) { ready = true; break; }
                        if (WaitForSingleObject(invocation.process.value, 0) == WAIT_OBJECT_0) break;
                        Sleep(5);
                    }
                    check(ready, "RealPayloadReady");
                    if (test.deny) check(invocation.captured.find("FILE_DENY read_error=5 execute_error=5") != std::string::npos, "FileDenyControlReceipt");
                    const bool violation = invocation.observe(test.budget);
                    exceeded = exceeded || violation;
                    std::printf("BUDGET_VIOLATION observed=%d\n", violation);
                    DWORD sent = 0;
                    require(WriteFile(invocation.input.value, "G", 1, &sent, nullptr) != FALSE && sent == 1, "ReleaseOwnPayload");
                    require(WaitForSingleObject(invocation.process.value, 10000) == WAIT_OBJECT_0, "RootExit");
                    DWORD code = 999;
                    require(GetExitCodeProcess(invocation.process.value, &code) != FALSE && code == 0, "RootExitCode");
                }
            } catch (const std::exception& error) {
                std::printf("CASE_INCOMPLETE operation=%s\n", error.what()); incomplete = true;
            }
            const bool cleanup_confirmed = invocation.cleanup();
            if (!cleanup_confirmed) incomplete = true;
            if (invocation.output.value) invocation.drain();
            std::printf("%s", invocation.captured.c_str());
            if (!cleanup_confirmed) break;
        }
        std::printf("RESULT atomic_assignment_ignored=%d budget_exceeded=%d incomplete=%d\n", assignment_ignored, exceeded, incomplete);
        return incomplete ? 2 : (exceeded || assignment_ignored) ? 1 : 0;
    } catch (const std::exception& error) {
        std::printf("REPRO_ERROR operation=%s\n", error.what()); return 2;
    }
}
