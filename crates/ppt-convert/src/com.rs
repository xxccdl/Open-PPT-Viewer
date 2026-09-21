//! COM 后期绑定（`IDispatch`）的一层极薄封装。
//!
//! # 为什么用后期绑定而不是 import 类型库
//!
//! WPS 和 Microsoft Office 的自动化对象模型**没有稳定的 IID 契约** ——
//! WPS 只保证 COM 名字（`Presentations.Open`、`SaveAs`…）与 Office 兼容，
//! 但类型库版本、接口 GUID 各家不同，甚至同一家不同版本也不同。
//! 生成绑定（`windows` 的 `#[implement]`/`import`）会把这层差异焊死在编译期，
//! 于是走 `IDispatch` 按**名字**调用：一套代码同时伺候 WPS 与 Office。
//!
//! # `VARIANT` 的构造为什么是手写的
//!
//! `windows` 只为少数类型实现了 `From<T> for VARIANT`，而我们需要精确控制
//! `vt` 与联合体字段。手写一个 8 行的构造函数，比在 `From` 覆盖面上赌运气可靠。
//! 所有构造出来的 `VARIANT` 都交给它自己的 `Drop`（内部即 `VariantClear`）释放，
//! 所有权单一，不泄漏也不重复释放。

use std::mem::ManuallyDrop;

use windows::core::{BSTR, GUID, PCWSTR};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{
    CLSIDFromProgID, CoCreateInstance, IDispatch, CLSCTX_LOCAL_SERVER, DISPATCH_FLAGS,
    DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPATCH_PROPERTYPUT, DISPPARAMS, EXCEPINFO,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_BSTR, VT_DISPATCH, VT_I4, VT_R4, VT_R8};

/// `DISPID_PROPERTYPUT`：属性写入时必须在命名参数里带上的保留 DISPID。
///
/// 这是 `IDispatch::Invoke` 协议里唯一一个由 OLE 硬编码的 DISPID，
/// `oaidl.h` 里定义为 `-3`，没有对应的 windows crate 常量。
const DISPID_PROPERTY_PUT: i32 = -3;

/// 转成以 NUL 结尾的宽字符串。
///
/// 不能把临时 `Vec` 的生命周期托付给调用者，所以调用点必须自己持有它。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 由 ProgID 取 CLSID。纯注册表查询，不会启动任何进程。
pub fn clsid_of_progid(progid: &str) -> Option<GUID> {
    let w = wide(progid);
    unsafe { CLSIDFromProgID(PCWSTR(w.as_ptr())).ok() }
}

/// 实例化一个自动化对象。
///
/// 用 `CLSCTX_LOCAL_SERVER`：WPS / Office 都注册为本地服务器（独立 exe），
/// 走 `INPROC_SERVER` 会拿不到。
pub fn create(progid: &str) -> Result<IDispatch, String> {
    let clsid = clsid_of_progid(progid)
        .ok_or_else(|| format!("系统里没有注册 {progid}（未安装对应办公软件）"))?;
    unsafe { CoCreateInstance::<_, IDispatch>(&clsid, None, CLSCTX_LOCAL_SERVER) }
        .map_err(|e| format!("无法启动 {progid}：{e}"))
}

/// 空的 `VARIANT`（`VT_EMPTY`），字段由调用方填。
///
/// `VARIANT_0` 的 `Anonymous` 是 `ManuallyDrop` 包着的，所以写字段必须先
/// 显式解引用一层 —— Rust 不会为 `ManuallyDrop` 自动套 `DerefMut`。
unsafe fn empty() -> VARIANT {
    std::mem::zeroed()
}

/// 构造 `VT_I4`。
pub fn v_i32(v: i32) -> VARIANT {
    let mut out = unsafe { empty() };
    unsafe {
        let body = &mut *out.Anonymous.Anonymous;
        body.vt = VT_I4;
        body.Anonymous.lVal = v;
    }
    out
}

/// 构造 `VT_BOOL`（自动化布尔是 `-1`/`0`，不是 `1`/`0`）。
pub fn v_bool(v: bool) -> VARIANT {
    let mut out = unsafe { empty() };
    unsafe {
        let body = &mut *out.Anonymous.Anonymous;
        body.vt = VT_BOOL;
        body.Anonymous.boolVal = VARIANT_BOOL(if v { -1 } else { 0 });
    }
    out
}

/// 构造 `VT_BSTR`。所有权交给 `VARIANT`，由它的 `Drop` 释放。
pub fn v_bstr(s: &str) -> VARIANT {
    let mut out = unsafe { empty() };
    unsafe {
        let body = &mut *out.Anonymous.Anonymous;
        body.vt = VT_BSTR;
        body.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
    }
    out
}

/// 一个后期绑定的自动化对象句柄。
pub struct Obj(pub IDispatch);

impl Clone for Obj {
    fn clone(&self) -> Self {
        Obj(self.0.clone())
    }
}

impl std::fmt::Debug for Obj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Obj(IDispatch)")
    }
}

impl Obj {
    /// 按名字取 DISPID。
    fn dispid(&self, name: &str) -> Result<i32, String> {
        let w = wide(name);
        let name_ptr = PCWSTR(w.as_ptr());
        let mut id = 0i32;
        unsafe {
            self.0
                .GetIDsOfNames(&GUID::zeroed(), &name_ptr, 1, 0, &mut id)
                .map_err(|e| format!("对象上没有成员「{name}」：{e}"))?;
        }
        Ok(id)
    }

    /// 调一次 `Invoke`。
    ///
    /// `args` 按**源码书写顺序**传，函数内部做倒序 —— `DISPPARAMS.rgvarg`
    /// 规定「最后一个参数在最前」，这是最容易写错的一处。
    fn invoke(
        &self,
        name: &str,
        flags: DISPATCH_FLAGS,
        args: Vec<VARIANT>,
        named: bool,
    ) -> Result<VARIANT, String> {
        let id = self.dispid(name)?;

        let mut ordered: Vec<VARIANT> = if named {
            args
        } else {
            args.into_iter().rev().collect()
        };
        let mut named_ids: Vec<i32> = if named {
            vec![DISPID_PROPERTY_PUT]
        } else {
            Vec::new()
        };

        let params = DISPPARAMS {
            rgvarg: ordered.as_mut_ptr(),
            rgdispidNamedArgs: named_ids.as_mut_ptr(),
            cArgs: ordered.len() as u32,
            cNamedArgs: named_ids.len() as u32,
        };

        let mut result = VARIANT::default();
        let mut excep = EXCEPINFO::default();

        let hr = unsafe {
            self.0.Invoke(
                id,
                &GUID::zeroed(),
                0,
                flags,
                &params,
                Some(&mut result),
                Some(&mut excep),
                None,
            )
        };

        // `ordered` / `named_ids` 必须活到 Invoke 之后，因此这里显式忽略未使用告警
        let _ = (&ordered, &named_ids);

        match hr {
            Ok(()) => Ok(result),
            Err(e) => {
                // 取走 BSTR，让它按自己的 Drop 释放（`EXCEPINFO` 本身不管理它）
                let desc = std::mem::take(&mut excep.bstrDescription);
                let text = desc.to_string();
                if text.is_empty() {
                    Err(format!("调用 {name} 失败：{e}"))
                } else {
                    Err(format!("调用 {name} 失败：{text}"))
                }
            }
        }
    }

    /// 读属性。
    pub fn get(&self, name: &str) -> Result<VARIANT, String> {
        self.invoke(name, DISPATCH_PROPERTYGET, Vec::new(), false)
    }

    /// 写属性。
    pub fn put(&self, name: &str, value: VARIANT) -> Result<(), String> {
        self.invoke(name, DISPATCH_PROPERTYPUT, vec![value], true)
            .map(|_| ())
    }

    /// 调方法。
    pub fn call(&self, name: &str, args: Vec<VARIANT>) -> Result<VARIANT, String> {
        self.invoke(name, DISPATCH_METHOD, args, false)
    }

    /// 读一个返回对象的属性，并转成 `Obj`。
    pub fn get_obj(&self, name: &str) -> Result<Obj, String> {
        let v = self.get(name)?;
        as_obj(&v).ok_or_else(|| format!("成员「{name}」没有返回对象"))
    }

    /// 取一个**带参数的属性**（典型例子：`TextRange2.Paragraphs(3)`）。
    ///
    /// # 为什么它和 `call_obj` 不是一回事
    ///
    /// 这类成员在类型库里是**属性**（`propget`），不是方法：
    /// 用 `DISPATCH_METHOD` 去调会直接被拒。而拒绝的表现是「什么都没发生」——
    /// 调用点若只是 `else { continue }`，就会一路静默地什么都不做，
    /// 日志里连一行都没有（这个坑踩过两次了）。
    ///
    /// 先按属性取，不成再按方法取：同一套代码要同时伺候 WPS 与 Office，
    /// 而两家的类型库不完全一样，赌一种不如两种都试一次。
    pub fn get_obj_with(&self, name: &str, args: Vec<VARIANT>) -> Result<Obj, String> {
        let v = self
            .invoke(name, DISPATCH_PROPERTYGET, args.clone(), false)
            .or_else(|_| self.invoke(name, DISPATCH_METHOD, args, false))?;
        as_obj(&v).ok_or_else(|| format!("成员「{name}」没有返回对象"))
    }

    /// 把一个 `VARIANT` 结果转成 `Obj`。
    pub fn call_obj(&self, name: &str, args: Vec<VARIANT>) -> Result<Obj, String> {
        let v = self.call(name, args)?;
        as_obj(&v).ok_or_else(|| format!("方法「{name}」没有返回对象"))
    }
}

/// 从 `VARIANT` 里取出 `IDispatch`（只对 `VT_DISPATCH` 成立）。
///
/// 取出的是 `IDispatch` 的一份 **AddRef 后的克隆**；
/// 原 `VARIANT` 仍由它自己的 `Drop` 负责释放，两边互不干扰。
fn as_obj(v: &VARIANT) -> Option<Obj> {
    unsafe {
        if v.Anonymous.Anonymous.vt != VT_DISPATCH {
            return None;
        }
        let slot = &v.Anonymous.Anonymous.Anonymous.pdispVal;
        let inner: &Option<IDispatch> = slot;
        inner.as_ref().map(|d| Obj(d.clone()))
    }
}

/// 从 `VARIANT` 里取出 `i32`（`VT_I4`）。
pub fn as_i32(v: &VARIANT) -> Option<i32> {
    unsafe {
        if v.Anonymous.Anonymous.vt != VT_I4 {
            return None;
        }
        Some(v.Anonymous.Anonymous.Anonymous.lVal)
    }
}

/// 构造 `VT_R4`（自动化里 `Single` 用它，例如文字填充的透明度）。
pub fn v_f32(v: f32) -> VARIANT {
    let mut out = unsafe { empty() };
    unsafe {
        let body = &mut *out.Anonymous.Anonymous;
        body.vt = VT_R4;
        body.Anonymous.fltVal = v;
    }
    out
}

/// 从 `VARIANT` 里取出浮点数（`VT_R4` 或 `VT_R8`）。
///
/// 两种都要认：`Transparency` 在类型库上是 `Single`，但服务端完全可能
/// 用 `Double` 回给我们 —— 只认一种就会读不到「原值」，
/// 于是还原时只能硬写成 0。
pub fn as_f32(v: &VARIANT) -> Option<f32> {
    unsafe {
        match v.Anonymous.Anonymous.vt {
            VT_R4 => Some(v.Anonymous.Anonymous.Anonymous.fltVal),
            VT_R8 => Some(v.Anonymous.Anonymous.Anonymous.dblVal as f32),
            _ => None,
        }
    }
}

/// 从 `VARIANT` 里取出自动化布尔。
///
/// # 为什么两种类型都要认
///
/// `Shape.Visible` 这类属性是 `MsoTriState`（枚举）：Office 读回来是
/// `VT_BOOL`，而 **WPS 读回来是 `VT_I4`**（实测）。只认前者的话，
/// 每个形状都会被当成「不可见」，于是整页一步都藏不住 ——
/// 出来的每一帧都和整页图一模一样，而日志里一句错都没有。
pub fn as_bool(v: &VARIANT) -> Option<bool> {
    unsafe {
        match v.Anonymous.Anonymous.vt {
            VT_BOOL => Some(v.Anonymous.Anonymous.Anonymous.boolVal.0 != 0),
            VT_I4 => Some(v.Anonymous.Anonymous.Anonymous.lVal != 0),
            _ => None,
        }
    }
}

/// `msoGroup`：组合形状的 `Shape.Type`。
const MSO_GROUP: i32 = 6;

/// 组合嵌套的深度上限。
///
/// 正常的课件不会有这么深的组合，这个数字防的是畸形文件把我们拖进死循环。
const MAX_GROUP_DEPTH: u32 = 8;

/// 收集一个形状集合里的全部形状（**含组合内部的**），
/// 返回 `(形状 id, 形状对象, 现在是否可见)`。
///
/// # 为什么要递归进组合
///
/// `Slide.Shapes` 只给出顶层形状。而动画的目标可能是组合里的某个子形状
/// （老师把几个图拼成一组再一起做动画，很常见）—— 不进去就找不到它，
/// 那一步就藏不住，于是「点下去发现内容早就显示了」。
///
/// # 为什么要把可见性一并读出来
///
/// 调用方只该动**本来可见**的形状。课件里 `hidden="1"` 的对象读出来就是
/// 不可见，若不加区分地隐藏/恢复，会把本不该出现的东西点亮。
/// 读不到时按「不可见」处理：宁可这一步不动它，也不要让它冒出来。
///
/// 失败一律跳过而不是上抛：某个形状读不出来不该让整页都出不了图。
pub fn collect_shapes(container: &Obj, out: &mut Vec<(i32, Obj, bool)>, depth: u32) {
    if depth > MAX_GROUP_DEPTH {
        return;
    }
    let count = container
        .get("Count")
        .ok()
        .and_then(|v| as_i32(&v))
        .unwrap_or(0);
    // COM 的集合都是 1-based
    for i in 1..=count {
        let Ok(shape) = container.call_obj("Item", vec![v_i32(i)]) else {
            continue;
        };
        let id = shape.get("Id").ok().and_then(|v| as_i32(&v)).unwrap_or(0);
        if id != 0 {
            let visible = shape
                .get("Visible")
                .ok()
                .and_then(|v| as_bool(&v))
                .unwrap_or(false);
            out.push((id, shape.clone(), visible));
        }
        if shape.get("Type").ok().and_then(|v| as_i32(&v)) == Some(MSO_GROUP) {
            if let Ok(items) = shape.get_obj("GroupItems") {
                collect_shapes(&items, out, depth + 1);
            }
        }
    }
}

/// 初始化当前线程为 STA。
///
/// WPS / Office 的自动化对象都要求单线程套间；在 MTA 里创建会拿到
/// `RPC_E_CHANGED_MODE`。
///
/// 返回 `true` 表示本次调用**成功**（`S_OK` 或 `S_FALSE`），
/// 两种情况都要配一次 `CoUninitialize` —— 这是 COM 的记账规则，
/// 不是「有没有真的初始化」的区分。
pub fn init_sta() -> bool {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok()
}

/// 撤销当前线程的 COM 初始化。
pub fn uninit() {
    unsafe { windows::Win32::System::Com::CoUninitialize() };
}

/// 办公软件可能的进程名（小写）。
///
/// WPS 系：`wpp.exe`（演示）、`wps.exe`（文字）、`et.exe`（表格）、
/// `wpscloudsvr.exe`（云服务）、`wpsoffice.exe`（启动器）、`ksomisc.exe`（辅助）。
/// Office 系：`powerpnt.exe`。
///
/// 只降这些名字的优先级，是为了**绝不误伤**用户自己的其它程序 ——
/// 快照差集只能告诉我们「哪些进程是刚起来的」，不能告诉我们「它们是谁」。
const OFFICE_EXES: &[&str] = &[
    "wpp.exe",
    "wps.exe",
    "et.exe",
    "wpscloudsvr.exe",
    "wpsoffice.exe",
    "ksomisc.exe",
    "powerpnt.exe",
];

/// 当前所有进程的 `(pid, 小写进程名)` 快照。
///
/// 用来做「创建 COM 对象前后」的差集，从而认出**我们刚拉起来的**那个进程。
/// 直接按名字找是不行的：老师可能自己正开着 WPS 改课件，
/// 把人家正在编辑的那份降到低优先级是很无礼的事。
pub fn snapshot_processes() -> Vec<(u32, String)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                out.push((
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase(),
                ));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    out
}

/// 把 `before` 之后新出现的办公软件进程降到「低于正常」优先级。
///
/// # 为什么必须这么做
///
/// 导出 PDF 是一次**满核**的活（实测 174MB / 39 页要 10 秒）。
/// 这段时间里老师已经在翻课件了 —— 如果导出进程和翻页抢 CPU，
/// 在大课件 + 老机器上就是「打开非常吃力、翻页特别卡」。
///
/// 降成 `BELOW_NORMAL` 之后，调度器会把 CPU 优先让给前台渲染，
/// 导出只是「用掉剩下的空档」，总时长略长但**用户完全感觉不到**。
///
/// 返回被成功降级的进程数（用于日志）。
pub fn lower_office_priority(before: &[(u32, String)]) -> usize {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS, PROCESS_SET_INFORMATION,
    };

    let known: std::collections::HashSet<u32> = before.iter().map(|(pid, _)| *pid).collect();
    let mut lowered = 0usize;

    for (pid, name) in snapshot_processes() {
        if known.contains(&pid) || !OFFICE_EXES.contains(&name.as_str()) {
            continue;
        }
        unsafe {
            // 拿不到句柄很正常（权限不足、进程已退出），跳过即可 ——
            // 这次优化失败不该影响转换本身
            if let Ok(handle) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) {
                if SetPriorityClass(handle, BELOW_NORMAL_PRIORITY_CLASS).is_ok() {
                    lowered += 1;
                }
                let _ = CloseHandle(handle);
            }
        }
    }

    lowered
}
