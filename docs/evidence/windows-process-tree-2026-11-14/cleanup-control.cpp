#define wmain diagnostic_wmain
#include "windows_psec_budget.cpp"
#undef wmain

int wmain(int argc, wchar_t**) {
    setvbuf(stdout, nullptr, _IONBF, 0);
    if (argc != 1) {
        std::puts("UNEXPECTED_PAYLOAD_EXECUTED");
        return 99;
    }
    try {
        PsecApi api;
        Invocation invocation(api);
        invocation.job.value = CreateJobObjectW(nullptr, nullptr);
        require(invocation.job.value != nullptr, "CreateEmptyOwnedJob");
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        require(SetInformationJobObject(invocation.job.value, JobObjectExtendedLimitInformation,
            &limits, sizeof(limits)) != FALSE, "SetEmptyJobKillOnClose");
        auto image = image_path();
        auto command = quote(image) + L" --must-never-execute";
        STARTUPINFOW startup{};
        startup.cb = sizeof(startup);
        PROCESS_INFORMATION info{};
        require(CreateProcessW(image.c_str(), command.data(), nullptr, nullptr, FALSE,
            CREATE_SUSPENDED | DETACHED_PROCESS, nullptr, nullptr, &startup, &info) != FALSE,
            "CreateFixedSuspendedControl");
        invocation.process.value = info.hProcess;
        invocation.thread.value = info.hThread;
        require(SuspendThread(invocation.thread.value) == 1 && ResumeThread(invocation.thread.value) == 2,
            "OriginalThreadStillSuspended");
        BOOL member = TRUE;
        require(IsProcessInJob(invocation.process.value, invocation.job.value, &member) != FALSE,
            "QueryEmptyJobMembership");
        check(member == FALSE, "IntentionallyOutsideOwnedJob");
        require(WaitForSingleObject(invocation.process.value, 0) == WAIT_TIMEOUT,
            "SuspendedControlAliveBeforeCleanup");
        std::puts("CONTROL suspended=1 member=0 resumed=0 retained_creation_handle=1");
        const bool accepted = invocation.cleanup();
        const DWORD wait = WaitForSingleObject(invocation.process.value, 0);
        DWORD code = STILL_ACTIVE;
        require(GetExitCodeProcess(invocation.process.value, &code) != FALSE,
            "ReadRetainedControlExitCode");
        check(!accepted, "FallbackMustRemainQualificationFailure");
        check(wait == WAIT_OBJECT_0 && code == 91, "FallbackTerminatedOriginalSuspendedProcess");
        std::printf("HARNESS_RESULT cleanup_accepted=%d retained_handle_signaled=%d exit_code=%lu payload_resumed=0\n",
            accepted, wait == WAIT_OBJECT_0, code);
        return 0;
    } catch (const std::exception& error) {
        std::printf("HARNESS_ERROR operation=%s\n", error.what());
        return 2;
    }
}
