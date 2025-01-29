use crate::warp_wrapper_path::get_warp_wrapper_path;
use windows::core::s;
use windows::core::PCSTR;
use windows::Win32::System::Registry;

pub fn run() {
    let mut key = Registry::HKEY::default();
    let mut disposition = Registry::REG_CREATE_KEY_DISPOSITION::default();

    unsafe {
        Registry::RegCreateKeyExA(
            Registry::HKEY_CURRENT_USER,
            s!("SOFTWARE\\Classes\\comms"),
            None,
            PCSTR::null(),
            Registry::REG_OPTION_NON_VOLATILE,
            Registry::KEY_WRITE,
            None,
            &mut key,
            Some(&mut disposition),
        )
        .ok()
        .unwrap();
    }

    unsafe {
        Registry::RegSetValueA(
            key,
            PCSTR::null(),
            Registry::REG_SZ,
            Some(s!("URL:comms").as_bytes()),
        )
        .ok()
        .unwrap();

        Registry::RegSetKeyValueA(
            key,
            PCSTR::null(),
            s!("URL Protocol"),
            Registry::REG_SZ.0,
            None,
            0,
        )
        .ok()
        .unwrap();

        Registry::RegCreateKeyExA(
            key,
            s!("shell\\open\\command"),
            None,
            PCSTR::null(),
            Registry::REG_OPTION_NON_VOLATILE,
            Registry::KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .unwrap();

        let comms_path = match get_warp_wrapper_path() {
            Some(path) => path.to_string_lossy().into_owned(),
            None => return,
        };

        Registry::RegSetValueA(
            key,
            PCSTR::null(),
            Registry::REG_SZ,
            Some(PCSTR::from_raw(format!("\"{comms_path}\" \"%1\"\0").as_ptr()).as_bytes()),
        )
        .ok()
        .unwrap();
    }
}
