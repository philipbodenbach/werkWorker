// Ephemeral USER objects owned by the verified ordinary diagnostic host.
#include <aclapi.h>
#pragma comment(lib, "user32.lib")

struct WriterDesktop {
    HWINSTA station = nullptr, original = nullptr;
    HDESK desktop = nullptr;
    std::wstring station_name, desktop_name, owner;
    bool switched = false;
    bool provisioned = false;

    void descriptor(bool is_station, const std::wstring& package, LocalBlock& sd) {
        const auto limited = is_station ? L"0x2037b" : L"0x20087";
        const auto full = is_station ? L"0xf037f" : L"0xf01ff";
        const auto user = package.empty() ? full : limited;
        // An explicit OWNER RIGHTS ACE bounds the same-account payload's implicit owner rights.
        std::wstring sddl = L"O:" + owner + L"D:P(A;;RC;;;OW)(A;;" + full + L";;;SY)(A;;" + full
            + L";;;BA)(A;;" + user + L";;;" + owner + L")";
        if (!package.empty()) {
            sddl += std::wstring(L"(A;;") + limited + L";;;" + package + L")(A;;" + limited
                + L";;;S-1-5-21-4275439644-3620842777-1714106797-1061)";
        }
        sddl += L"S:(ML;;NW;;;LW)";
        require(ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.c_str(), SDDL_REVISION_1,
            &sd.p, nullptr), "BuildPrivateUserDescriptor");
    }

    void create(const std::wstring& nonce, const std::wstring& user_sid) {
        owner = user_sid;
        station_name = L"ww-writer-station-" + nonce;
        desktop_name = L"ww-writer-desktop-" + nonce;
        LocalBlock ws, ds;
        descriptor(true, L"", ws);
        descriptor(false, L"", ds);
        SECURITY_ATTRIBUTES sw{sizeof(sw), ws.p, FALSE}, sd{sizeof(sd), ds.p, FALSE};
        // CWF_CREATE_ONLY prevents adopting an existing object.
        station = CreateWindowStationW(station_name.c_str(), 1, 0xf037f, &sw);
        require(station != nullptr, "CreatePrivateWriterStation");
        original = GetProcessWindowStation();
        require(original && SetProcessWindowStation(station), "SelectPrivateWriterStation");
        switched = true;
        desktop = CreateDesktopExW(desktop_name.c_str(), nullptr, nullptr, 0, 0xf01ff, &sd, 0, nullptr);
        require(desktop != nullptr, "CreatePrivateWriterDesktop");
        require(SetProcessWindowStation(original), "RestoreOriginalStation");
        switched = false;
    }

    void open_provisioned(const std::wstring& nonce, const std::wstring& user_sid) {
        provisioned = true;
        owner = user_sid;
        station_name = L"ww-ui-" + nonce;
        desktop_name = L"desk-" + nonce;
        station = OpenWindowStationW(station_name.c_str(), FALSE, 0xf037f);
        require(station != nullptr, "OpenFreshProvisionedStation");
        original = GetProcessWindowStation();
        require(original && SetProcessWindowStation(station), "SelectFreshProvisionedStation");
        switched = true;
        desktop = OpenDesktopW(desktop_name.c_str(), 0, FALSE, 0xf01ff);
        require(desktop != nullptr, "OpenFreshProvisionedDesktop");
        require(SetProcessWindowStation(original), "RestoreOriginalStation");
        switched = false;
        for (bool ws : {true, false}) {
            HANDLE object = ws ? static_cast<HANDLE>(station) : static_cast<HANDLE>(desktop);
            PSID object_owner = nullptr;
            LocalBlock info;
            check(GetSecurityInfo(object, SE_WINDOW_OBJECT, OWNER_SECURITY_INFORMATION, &object_owner,
                nullptr, nullptr, nullptr, &info.p) == ERROR_SUCCESS && sid_text(object_owner) == L"S-1-5-32-544",
                "AdministratorOwnedPrivateUserObject");
            wchar_t actual[128]{};
            DWORD length = 0;
            require(GetUserObjectInformationW(object, UOI_NAME, actual, sizeof(actual), &length)
                && (ws ? station_name : desktop_name) == actual, "BoundPrivateUserObjectName");
        }
    }

    void bind(const std::wstring& package) {
        for (bool ws : {true, false}) {
            LocalBlock desired, observed;
            descriptor(ws, package, desired);
            HANDLE object = ws ? static_cast<HANDLE>(station) : static_cast<HANDLE>(desktop);
            PACL acl = nullptr, actual = nullptr, labels = nullptr;
            BOOL present = FALSE, defaulted = FALSE;
            require(GetSecurityDescriptorDacl(desired.p, &present, &acl, &defaulted) && present && acl,
                "ExpectedPrivateUserAcl");
            check(SetSecurityInfo(object, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                nullptr, nullptr, acl, nullptr) == ERROR_SUCCESS, "ApplyPrivateUserDacl");
            PSID actual_owner = nullptr;
            check(GetSecurityInfo(object, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                &actual_owner, nullptr, &actual, &labels, &observed.p) == ERROR_SUCCESS, "ReadPrivateUserAcl");
            check(actual && sid_text(actual_owner) == (provisioned ? L"S-1-5-32-544" : owner)
                && actual->AceCount == acl->AceCount, "PrivateUserOwnerAndAces");
            for (DWORD i = 0; i < acl->AceCount; ++i) {
                void *a = nullptr, *b = nullptr;
                require(GetAce(acl, i, &a) && GetAce(actual, i, &b), "PrivateUserAce");
                auto bytes = static_cast<ACE_HEADER*>(a)->AceSize;
                check(static_cast<ACE_HEADER*>(b)->AceSize == bytes && memcmp(a, b, bytes) == 0, "ExactPrivateUserAce");
            }
            void* value = nullptr;
            check(labels && labels->AceCount == 1 && GetAce(labels, 0, &value), "PrivateUserLabel");
            auto* label = static_cast<SYSTEM_MANDATORY_LABEL_ACE*>(value);
            check(label->Header.AceType == SYSTEM_MANDATORY_LABEL_ACE_TYPE && label->Mask == SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
                && sid_text(&label->SidStart) == L"S-1-16-4096", "PrivateUserLowLabel");
        }
        std::printf("PRIVATE_USER_OBJECTS bound=1 station=%ls desktop=%ls\n", station_name.c_str(), desktop_name.c_str());
    }

    ~WriterDesktop() {
        if (switched) SetProcessWindowStation(original);
        if (desktop) std::printf("PRIVATE_DESKTOP closed=%d\n", CloseDesktop(desktop) != FALSE);
        if (station) std::printf("PRIVATE_STATION closed=%d\n", CloseWindowStation(station) != FALSE);
    }
};
