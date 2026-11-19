//! GPU 编码可行性探针：枚举本机所有 H.264 硬件编码器，逐个尝试激活，
//! 并打印 HRESULT，用于判断 NVENC / QSV 到底卡在哪一步。
//!
//! 运行：cargo run --release -p lite-core --example gpu_probe

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    MFStartup, MFTEnumEx, IMFActivate, IMFTransform, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MF_VERSION, MFSTARTUP_LITE,
    MFT_REGISTER_TYPE_INFO,
};

fn main() {
    unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        let in_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let out_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        for (label, flags) in [
            ("ALL", MFT_ENUM_FLAG_ALL),
            ("HARDWARE", MFT_ENUM_FLAG_HARDWARE),
            ("HARDWARE|SORT", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER),
        ] {
            let mut acts: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            let hr = MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&in_type),
                Some(&out_type),
                &mut acts,
                &mut count,
            );
            println!("=== flags {label}: hr={hr:?} count={count}");
            if count == 0 || acts.is_null() {
                continue;
            }
            let list = std::slice::from_raw_parts(acts, count as usize);
            for slot in list {
                let Some(activate) = slot else { continue };
                let name = friendly(activate);
                let transform: Result<IMFTransform, _> = activate.ActivateObject();
                match transform {
                    Ok(_) => println!("  [OK ] {name}"),
                    Err(e) => println!("  [ERR] {name} -> {e:?} / code=0x{:08X}", e.code().0 as u32),
                }
            }
        }
    }
}

unsafe fn friendly(activate: &IMFActivate) -> String {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    if activate
        .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf[..], Some(&mut len))
        .is_ok()
        && len > 0
    {
        let n = (len as usize).min(buf.len());
        return String::from_utf16_lossy(&buf[..n.saturating_sub(1)]);
    }
    "unknown".into()
}
