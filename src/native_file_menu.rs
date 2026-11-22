//! System metadata and execution for Filetree’s GPUI file menus.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use block::ConcreteBlock;
use cocoa::{
    base::{BOOL, YES, id, nil},
    foundation::{NSPoint, NSRect, NSSize, NSString},
};
use futures::channel::oneshot;
use objc::{class, msg_send, rc::StrongPtr, runtime::Class, sel, sel_impl};
use serde::Deserialize;

use crate::file_ops::file_url;

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSURLTypeIdentifierKey: id;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSFilenamesPboardType: id;
    fn NSPerformService(name: id, pasteboard: id) -> BOOL;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn UTTypeConformsTo(content_type: id, parent_type: id) -> u8;
}

#[link(name = "Automator", kind = "framework")]
unsafe extern "C" {
    #[link_name = "OBJC_CLASS_$_AMWorkflow"]
    static AM_WORKFLOW: Class;
}

/// The only native UI here is the tag-entry dialog and the selected share service.
pub(super) fn request_tag() -> oneshot::Receiver<Option<String>> {
    later(ask_tag)
}

pub(super) struct ShareChoice {
    pub index: usize,
    pub title: String,
    pub icon: Option<Arc<gpui::RenderImage>>,
}

pub(super) fn share_choices(paths: &[PathBuf]) -> Vec<ShareChoice> {
    // SAFETY: AppKit objects are queried on the main thread.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let urls = file_urls(manager, paths);
        let services: id = msg_send![class!(NSSharingService), sharingServicesForItems: urls];
        let count: usize = msg_send![services, count];
        (0..count)
            .filter_map(|index| {
                let service: id = msg_send![services, objectAtIndex: index];
                let title = crate::system::string(msg_send![service, menuItemTitle])?;
                let image: id = msg_send![service, image];
                // Enough resolution for the 16-point menu icon on Retina
                // displays, including UI zoom.
                let icon = crate::native_icons::rasterize_image(image, 48).ok();
                Some(ShareChoice { index, title, icon })
            })
            .collect()
    }
}

pub(super) fn share(
    paths: Vec<PathBuf>,
    index: usize,
    title: String,
) -> oneshot::Receiver<Result<()>> {
    later(move || {
        // SAFETY: Resolving and performing a sharing service use the main thread.
        unsafe {
            let manager: id = msg_send![class!(NSFileManager), defaultManager];
            let urls = file_urls(manager, &paths);
            let services: id = msg_send![class!(NSSharingService), sharingServicesForItems: urls];
            let count: usize = msg_send![services, count];
            ensure!(index < count, "The sharing option is no longer available");
            let service: id = msg_send![services, objectAtIndex: index];
            let current = crate::system::string(msg_send![service, menuItemTitle]);
            ensure!(
                current.as_deref() == Some(&title),
                "The sharing option is no longer available"
            );
            let _: () = msg_send![service, performWithItems: urls];
            Ok(())
        }
    })
}

pub(super) fn perform_service(name: String, paths: Vec<PathBuf>) -> oneshot::Receiver<Result<()>> {
    later(move || {
        // SAFETY: AppKit receives file URLs on an isolated, short-lived pasteboard.
        unsafe {
            let manager: id = msg_send![class!(NSFileManager), defaultManager];
            let urls = file_urls(manager, &paths);
            let board: id = msg_send![class!(NSPasteboard), pasteboardWithUniqueName];
            let written: BOOL = msg_send![board, writeObjects: urls];
            let path = StrongPtr::new(NSString::alloc(nil).init_str("path"));
            let names: id = msg_send![urls, valueForKey: *path];
            let legacy_type: id =
                msg_send![class!(NSArray), arrayWithObject: NSFilenamesPboardType];
            let _: isize = msg_send![board, addTypes: legacy_type owner: nil];
            let legacy: BOOL =
                msg_send![board, setPropertyList: names forType: NSFilenamesPboardType];
            let title = StrongPtr::new(NSString::alloc(nil).init_str(&name));
            let performed =
                (written == YES || legacy == YES) && NSPerformService(*title, board) == YES;
            let _: () = msg_send![board, releaseGlobally];
            ensure!(
                performed,
                "The service could not run. Check that it is enabled in macOS."
            );
            Ok(())
        }
    })
}

fn ask_tag() -> Option<String> {
    // SAFETY: AppKit creates a short-lived modal prompt on the main thread.
    unsafe {
        let field: id = msg_send![class!(NSTextField), alloc];
        let rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(230., 22.));
        let field = StrongPtr::new(msg_send![field, initWithFrame: rect]);
        let alert = StrongPtr::new(msg_send![class!(NSAlert), new]);
        let title = StrongPtr::new(NSString::alloc(nil).init_str("Add Tag"));
        let add = StrongPtr::new(NSString::alloc(nil).init_str("Add"));
        let cancel = StrongPtr::new(NSString::alloc(nil).init_str("Cancel"));
        let _: () = msg_send![*alert, setMessageText: *title];
        let _: () = msg_send![*alert, setAccessoryView: *field];
        let _: id = msg_send![*alert, addButtonWithTitle: *add];
        let _: id = msg_send![*alert, addButtonWithTitle: *cancel];
        let window: id = msg_send![*alert, window];
        let _: () = msg_send![window, setInitialFirstResponder: *field];
        let result: isize = msg_send![*alert, runModal];
        (result == 1000)
            .then(|| crate::system::string(msg_send![*field, stringValue]))
            .flatten()
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
    }
}

fn later<T: 'static>(task: impl FnOnce() -> T + 'static) -> oneshot::Receiver<T> {
    let (send, receive) = oneshot::channel();
    let work = RefCell::new(Some((task, send)));
    // SAFETY: NSOperationQueue retains its copied block and invokes it once
    // on the main thread, after the current event has returned.
    unsafe {
        let block = ConcreteBlock::new(move || {
            if let Some((task, send)) = work.borrow_mut().take() {
                let _ = send.send(task());
            }
        })
        .copy();
        let queue: id = msg_send![class!(NSOperationQueue), mainQueue];
        let _: () = msg_send![queue, addOperationWithBlock: &*block];
    }
    receive
}

unsafe fn file_urls(manager: id, paths: &[PathBuf]) -> id {
    // SAFETY: The caller holds an autorelease pool and has a live manager.
    unsafe {
        let urls: id = msg_send![class!(NSMutableArray), array];
        for path in paths {
            if let Some(url) = file_url(manager, path) {
                let _: () = msg_send![urls, addObject: url];
            }
        }
        urls
    }
}

#[derive(Deserialize)]
struct WorkflowService {
    #[serde(rename = "NSMenuItem")]
    menu: std::collections::HashMap<String, String>,
    #[serde(default, rename = "NSSendFileTypes")]
    file_types: Vec<String>,
    #[serde(default, rename = "NSSendTypes")]
    send_types: Vec<String>,
    #[serde(default, rename = "NSReturnTypes")]
    return_types: Vec<String>,
    #[serde(default, rename = "NSRequiredContext")]
    context: serde_json::Value,
}

pub(super) struct Workflow {
    pub title: String,
    pub path: PathBuf,
}

/// File-input services declared by installed apps and service bundles.
pub(super) fn services(home: &Path, selected: &[PathBuf]) -> Vec<String> {
    let bundle_id = bundle_identifier();
    let application = bundle_id.as_deref().into_iter().collect::<Vec<_>>();
    let roots = [
        home.join("Applications"),
        home.join("Library/Services"),
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/Library/Services"),
        PathBuf::from("/System/Library/Services"),
    ];
    let mut names = BTreeSet::new();
    for root in roots {
        service_manifests(&root, 2, &mut |bundle| {
            for service in read_manifest(bundle) {
                if service.return_types.is_empty()
                    && application_context(&service.context, &application)
                    && service.accepts_selection(selected)
                    && let Some(title) = service.menu.get("default")
                {
                    names.insert(title.clone());
                }
            }
        });
    }
    names.into_iter().collect()
}

fn bundle_identifier() -> Option<String> {
    // SAFETY: Foundation's bundle metadata API is safe to read on this thread.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        crate::system::string(msg_send![bundle, bundleIdentifier])
    }
}

fn service_manifests(folder: &Path, depth: usize, visit: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("app" | "service" | "workflow") => visit(&path),
            _ if depth > 0 && entry.file_type().is_ok_and(|kind| kind.is_dir()) => {
                service_manifests(&path, depth - 1, visit);
            }
            _ => {}
        }
    }
}

impl WorkflowService {
    fn accepts_selection(&self, selected: &[PathBuf]) -> bool {
        if self.file_types.is_empty() {
            return !selected.is_empty()
                && self
                    .send_types
                    .iter()
                    .any(|kind| kind == "NSFilenamesPboardType" || kind == "public.file-url");
        }
        accepts_files(selected, &self.file_types)
    }
}

fn read_manifest(bundle: &Path) -> Vec<WorkflowService> {
    read_manifest_inner(bundle).unwrap_or_default()
}

fn read_manifest_inner(bundle: &Path) -> Option<Vec<WorkflowService>> {
    // SAFETY: Foundation reads the plist on this thread. The JSON bytes are
    // used only while the autoreleased data remains live.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let url = file_url(manager, &bundle.join("Contents/Info.plist"))?;
        let plist: id = msg_send![class!(NSDictionary), dictionaryWithContentsOfURL: url];
        let key = StrongPtr::new(NSString::alloc(nil).init_str("NSServices"));
        let services: id = msg_send![plist, objectForKey: *key];
        let valid: BOOL = msg_send![class!(NSJSONSerialization), isValidJSONObject: services];
        if services == nil || valid != YES {
            return None;
        }
        let json: id = msg_send![class!(NSJSONSerialization), dataWithJSONObject: services options: 0_usize error: nil];
        let len: usize = msg_send![json, length];
        let bytes: *const u8 = msg_send![json, bytes];
        if bytes.is_null() || len == 0 {
            return None;
        }
        let items: Vec<serde_json::Value> =
            serde_json::from_slice(std::slice::from_raw_parts(bytes, len)).ok()?;
        Some(
            items
                .into_iter()
                .filter_map(|item| serde_json::from_value(item).ok())
                .collect(),
        )
    }
}

/// Offers installed Automator workflows that accept the current file selection.
pub(super) fn workflows(home: &Path, selected: &[PathBuf]) -> Vec<Workflow> {
    let folders = [
        home.join("Library/Services"),
        PathBuf::from("/Library/Services"),
    ];
    let mut workflows = Vec::new();
    for entry in folders
        .iter()
        .filter_map(|folder| fs::read_dir(folder).ok())
        .flatten()
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("workflow") {
            continue;
        }
        for service in read_manifest(&path) {
            if let Some(title) = service.menu.get("default")
                && application_context(&service.context, &["com.apple.finder"])
                && service.accepts_selection(selected)
            {
                workflows.push(Workflow {
                    title: title.clone(),
                    path: path.clone(),
                });
            }
        }
    }
    workflows.sort_by_key(|workflow| workflow.title.to_lowercase());
    workflows
}

fn application_context(context: &serde_json::Value, accepted: &[&str]) -> bool {
    match context {
        serde_json::Value::Null => true,
        serde_json::Value::Array(contexts) => contexts
            .iter()
            .any(|context| application_context(context, accepted)),
        serde_json::Value::Object(context) => {
            context
                .get("NSApplicationIdentifier")
                .is_none_or(|application| {
                    let matches = |id: &serde_json::Value| {
                        id.as_str().is_some_and(|name| accepted.contains(&name))
                    };
                    matches(application)
                        || application
                            .as_array()
                            .is_some_and(|ids| ids.iter().any(matches))
                })
        }
        _ => false,
    }
}

fn accepts_files(paths: &[PathBuf], accepted: &[String]) -> bool {
    if paths.is_empty() || accepted.is_empty() {
        return false;
    }
    if accepted.iter().any(|kind| kind == "public.item") {
        return true;
    }
    // SAFETY: Foundation and CoreServices read file type identifiers in this pool.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let types: Vec<_> = accepted
            .iter()
            .map(|kind| StrongPtr::new(NSString::alloc(nil).init_str(kind)))
            .collect();
        paths.iter().all(|path| {
            let Some(url) = file_url(manager, path) else { return false; };
            let mut kind: id = nil;
            let _: BOOL = msg_send![url, getResourceValue: &mut kind forKey: NSURLTypeIdentifierKey error: nil];
            kind != nil && types.iter().any(|parent| UTTypeConformsTo(kind, **parent) != 0)
        })
    }
}

pub(super) fn run_workflow(workflow: &Path, paths: &[PathBuf]) -> Result<()> {
    // SAFETY: Automator accepts an array of paths and runs the workflow in a
    // separate process. Foundation objects live for the duration of this call.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let url = file_url(manager, workflow).context("Could not read the workflow path")?;
        let urls = file_urls(manager, paths);
        let key = StrongPtr::new(NSString::alloc(nil).init_str("path"));
        let names: id = msg_send![urls, valueForKey: *key];
        let mut error: id = nil;
        let _: id =
            msg_send![&AM_WORKFLOW, runWorkflowAtURL: url withInput: names error: &mut error];
        if error != nil {
            let detail = crate::system::string(msg_send![error, localizedDescription]);
            bail!(
                "{}",
                detail.unwrap_or_else(|| "The workflow could not run".into())
            );
        }
    }
    Ok(())
}

pub(super) struct Shortcut {
    pub title: String,
    pub identifier: String,
}

pub(super) fn shortcuts() -> Result<Vec<Shortcut>> {
    let output = Command::new("/usr/bin/shortcuts")
        .args(["list", "--show-identifiers"])
        .output()?;
    ensure!(output.status.success(), "Could not list shortcuts");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (title, identifier) = line
                .strip_suffix(')')
                .and_then(|line| line.rsplit_once(" ("))
                .filter(|(_, identifier)| {
                    identifier.len() == 36
                        && identifier
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
                })
                .unwrap_or((line, line));
            Shortcut {
                title: title.to_owned(),
                identifier: identifier.to_owned(),
            }
        })
        .collect())
}

pub(super) fn run_shortcut(identifier: &str, paths: &[PathBuf]) -> Result<()> {
    let output = Command::new("/usr/bin/shortcuts")
        .arg("run")
        .arg(identifier)
        .arg("--input-path")
        .args(paths)
        .stdout(Stdio::null())
        .output()?;
    ensure!(
        output.status.success(),
        "The shortcut could not run. {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
