// Only fresh subdirectories of the pinned fixture's workspace and scratch roots.
#include <aclapi.h>

struct LocalBlock {
    void* p = nullptr;
    ~LocalBlock() { LocalFree(p); }
};

static std::wstring sid_text(PSID sid) {
    LPWSTR value = nullptr;
    require(ConvertSidToStringSidW(sid, &value), "ReadSid");
    std::wstring result(value);
    LocalFree(value);
    return result;
}

struct WriterDirectory {
    Handle directory;
    std::wstring path;
    bool created = false;

    static std::wstring acl_text(const std::wstring& user, const std::wstring& package) {
        return L"D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;" + user
            + L")(A;OICI;0x1301bf;;;S-1-5-21-4275439644-3620842777-1714106797-1061)"
            + (package.empty() ? std::wstring() : L"(A;OICI;0x1301bf;;;" + package + L")")
            + L"S:(ML;OICI;NW;;;LW)";
    }

    void create(const std::wstring& name, const std::wstring& user) {
        path = name;
        LocalBlock descriptor;
        require(ConvertStringSecurityDescriptorToSecurityDescriptorW(acl_text(user, L"").c_str(),
            SDDL_REVISION_1, &descriptor.p, nullptr), "BuildPrivateDirectoryDescriptor");
        SECURITY_ATTRIBUTES sa{sizeof(sa), descriptor.p, FALSE};
        require(CreateDirectoryW(path.c_str(), &sa), "RequireFreshPrivateDirectory");
        created = true;
        directory.value = CreateFileW(path.c_str(), READ_CONTROL | WRITE_DAC, FILE_SHARE_READ | FILE_SHARE_WRITE,
            nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
        require(directory.value != INVALID_HANDLE_VALUE, "PinPrivateDirectory");
    }

    void bind(const std::wstring& user, const std::wstring& package) {
        LocalBlock descriptor;
        require(ConvertStringSecurityDescriptorToSecurityDescriptorW(acl_text(user, package).c_str(),
            SDDL_REVISION_1, &descriptor.p, nullptr), "BuildBoundDirectoryDescriptor");
        BOOL present = FALSE, defaulted = FALSE;
        PACL desired = nullptr;
        require(GetSecurityDescriptorDacl(descriptor.p, &present, &desired, &defaulted) && present && desired,
            "ReadExpectedPrivateDacl");
        check(SetSecurityInfo(directory.value, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            nullptr, nullptr, desired, nullptr) == ERROR_SUCCESS, "SetPrivateDirectoryDacl");
        LocalBlock actual;
        PACL dacl = nullptr, labels = nullptr;
        check(GetSecurityInfo(directory.value, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            nullptr, nullptr, &dacl, &labels, &actual.p) == ERROR_SUCCESS, "ReadBoundDirectoryDescriptor");
        check(dacl && dacl->AceCount == desired->AceCount, "PrivateAceCount");
        for (DWORD i = 0; i < desired->AceCount; ++i) {
            void *expected = nullptr, *observed = nullptr;
            require(GetAce(desired, i, &expected) && GetAce(dacl, i, &observed), "ReadPrivateAce");
            const auto bytes = static_cast<ACE_HEADER*>(expected)->AceSize;
            check(bytes == static_cast<ACE_HEADER*>(observed)->AceSize && memcmp(expected, observed, bytes) == 0,
                "PrivateAceReadback");
        }
        void* label = nullptr;
        check(labels && labels->AceCount == 1 && GetAce(labels, 0, &label), "ReadPrivateLabel");
        auto* ace = static_cast<SYSTEM_MANDATORY_LABEL_ACE*>(label);
        check(ace->Header.AceType == SYSTEM_MANDATORY_LABEL_ACE_TYPE && ace->Mask == SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
            && sid_text(&ace->SidStart) == L"S-1-16-4096", "PrivateLowLabel");
        std::printf("PRIVATE_DIRECTORY verified=1 path=%ls writer_modify=1 package_modify=1 low=1\n", path.c_str());
    }

    ~WriterDirectory() {
        directory.close();
        // Only an empty directory created by this invocation can be removed.
        if (created) std::printf("PRIVATE_DIRECTORY_CLOSE removed=%d\n", RemoveDirectoryW(path.c_str()) != FALSE);
    }
};
