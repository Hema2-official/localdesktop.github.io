//! Clients' dma-bufs (zwp_linux_dmabuf_v1) on Android's GPU. Android's EGL imports only Android's
//! own buffers (AHardwareBuffers, through EGL_ANDROID_image_native_buffer), not dma-bufs. A
//! dma-buf becomes one with the handle of a buffer gralloc makes of the same size and format: the
//! handle goes through a socket the way Android passes buffers between processes, with the
//! dma-buf's descriptor in place of the pixels'. Nothing else in it changes, as its layout is the
//! vendor's.
//!
//! That shows the client's buffer itself where gralloc lays its buffer out the same: rows as far
//! apart, and no more bytes. Turnip's linear images match Qualcomm's gralloc but where their rows
//! are a multiple of 512 pixels long, which gralloc pads further. Such a buffer becomes a buffer
//! of bytes instead (a gralloc BLOB's handle, GL_EXT_external_buffer), which the GPU copies into
//! a buffer of the app's at each commit, row by row.

use smithay::backend::{
    allocator::{
        dmabuf::{Dmabuf, WeakDmabuf},
        Buffer, Format, Fourcc, Modifier,
    },
    egl::{
        display::{set_dmabuf_image_importer, DmabufImageImporter},
        ffi::{self, egl::types::EGLImage},
        get_proc_address, EGLDisplay, Error as EglError,
    },
    renderer::gles::ffi as gl,
};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::{Mutex, OnceLock};

/// The formats clients may hand over, all linear (`Modifier::Linear`).
const FORMATS: [Fourcc; 4] = [
    Fourcc::Argb8888,
    Fourcc::Xrgb8888,
    Fourcc::Abgr8888,
    Fourcc::Xbgr8888,
];

// AHardwareBuffer (NDK, API level 26), looked up at run time: the app's minimum level is lower.
#[repr(C)]
#[derive(Default)]
struct BufferDesc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}

const FORMAT_R8G8B8A8_UNORM: u32 = 1;
const FORMAT_R8G8B8X8_UNORM: u32 = 2;
/// Android's BGRA_8888, which gralloc takes though the NDK doesn't name it.
const FORMAT_B8G8R8A8_UNORM: u32 = 5;
const FORMAT_BLOB: u32 = 0x21;
/// CPU access keeps gralloc's layout linear.
const USAGE_CPU_READ_RARELY: u64 = 2;
const USAGE_GPU_SAMPLED_IMAGE: u64 = 1 << 8;
const USAGE_GPU_FRAMEBUFFER: u64 = 1 << 9;
const USAGE_GPU_DATA_BUFFER: u64 = 1 << 24;

const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
const EGL_TRUE: i32 = 1;
const EGL_NONE: i32 = 0x3038;

/// The most descriptors a buffer's handle has.
const MAX_HANDLE_FDS: usize = 16;

/// The AHardwareBuffer format with a DRM format's layout in memory. Android has no BGRX: X
/// formats leave the alpha unused either way.
fn hardware_buffer_format(code: Fourcc) -> Option<u32> {
    match code {
        Fourcc::Abgr8888 => Some(FORMAT_R8G8B8A8_UNORM),
        Fourcc::Xbgr8888 => Some(FORMAT_R8G8B8X8_UNORM),
        Fourcc::Argb8888 | Fourcc::Xrgb8888 => Some(FORMAT_B8G8R8A8_UNORM),
        _ => None,
    }
}

/// Whether a format's bytes are in blue, green, red order.
fn is_bgra(code: Fourcc) -> bool {
    matches!(code, Fourcc::Argb8888 | Fourcc::Xrgb8888)
}

/// Whether a client's buffer is one the importer may take: one linear plane of one of `FORMATS`,
/// from the start of its dma-buf.
pub fn can_import(dmabuf: &Dmabuf) -> bool {
    let format = dmabuf.format();
    dmabuf.num_planes() == 1
        && format.modifier == Modifier::Linear
        && FORMATS.contains(&format.code)
        && dmabuf.offsets().next() == Some(0)
}

/// The formats to offer clients.
pub fn formats() -> impl Iterator<Item = Format> {
    FORMATS.into_iter().map(|code| Format {
        code,
        modifier: Modifier::Linear,
    })
}

struct Ndk {
    allocate: unsafe extern "C" fn(*const BufferDesc, *mut *mut c_void) -> i32,
    describe: unsafe extern "C" fn(*const c_void, *mut BufferDesc),
    release: unsafe extern "C" fn(*mut c_void),
    send_handle: unsafe extern "C" fn(*const c_void, i32) -> i32,
    receive_handle: unsafe extern "C" fn(i32, *mut *mut c_void) -> i32,
    _library: libloading::Library,
}

impl Ndk {
    fn load() -> Result<Self, libloading::Error> {
        unsafe {
            let library = libloading::Library::new("libandroid.so")?;
            Ok(Self {
                allocate: *library.get(b"AHardwareBuffer_allocate\0")?,
                describe: *library.get(b"AHardwareBuffer_describe\0")?,
                release: *library.get(b"AHardwareBuffer_release\0")?,
                send_handle: *library.get(b"AHardwareBuffer_sendHandleToUnixSocket\0")?,
                receive_handle: *library.get(b"AHardwareBuffer_recvHandleFromUnixSocket\0")?,
                _library: library,
            })
        }
    }

    /// A new buffer, and its description with the stride gralloc chose.
    fn allocate(&self, mut desc: BufferDesc) -> Result<(HardwareBuffer, BufferDesc), String> {
        let mut buffer = ptr::null_mut();
        if unsafe { (self.allocate)(&desc, &mut buffer) } != 0 {
            return Err(format!(
                "gralloc has no {}x{} buffer of format {}",
                desc.width, desc.height, desc.format
            ));
        }
        unsafe { (self.describe)(buffer, &mut desc) };
        Ok((HardwareBuffer(buffer), desc))
    }

    fn release(&self, buffer: HardwareBuffer) {
        unsafe { (self.release)(buffer.0) };
    }
}

/// An AHardwareBuffer the importer holds a reference to.
#[derive(Clone, Copy)]
struct HardwareBuffer(*mut c_void);

/// A buffer's handle as `AHardwareBuffer_sendHandleToUnixSocket` sends it.
struct Handle {
    data: Vec<u8>,
    fds: Vec<OwnedFd>,
}

/// What an image shows.
enum Contents {
    /// The client's buffer itself.
    Wrapped(HardwareBuffer),
    /// A buffer of the app's, which the client's is copied into at each commit.
    Copied(Copy),
}

/// A client's buffer the GPU copies into one of the app's.
struct Copy {
    /// The client's buffer, as bytes.
    source: HardwareBuffer,
    /// The image's buffer.
    target: HardwareBuffer,
    width: i32,
    height: i32,
    /// How far apart the client's rows are, in pixels.
    row_length: i32,
    /// The client's buffer's size in bytes.
    size: i64,
    /// The GL format of the client's pixels.
    format: u32,
    /// GL's buffer of `source` and a texture of `target`, made at the first commit, which has the
    /// renderer's context current.
    objects: Option<(u32, u32)>,
}

/// The GL functions the copies take.
struct Gl {
    gles: gl::Gles2,
    buffer_storage_external: unsafe extern "C" fn(u32, isize, isize, *mut c_void, u32),
}

struct Importer {
    ndk: Ndk,
    get_native_client_buffer: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    /// Loaded at the first copy; none if GL lacks GL_EXT_external_buffer.
    gl: OnceLock<Option<Gl>>,
    /// What the images made so far show, released with the images.
    images: Mutex<HashMap<usize, Contents>>,
    /// Buffers that failed to import, which aren't tried again.
    failed: Mutex<Vec<WeakDmabuf>>,
    /// The messages logged so far, each once.
    logged: Mutex<HashSet<String>>,
}

// The buffers and GL's functions are used where the renderer is, one thread at a time, but the
// importer is global.
unsafe impl Send for Importer {}
unsafe impl Sync for Importer {}

impl Importer {
    fn log_once(&self, message: String) {
        if self.logged.lock().unwrap().insert(message.clone()) {
            log::info!("Graphics: {message}");
        }
    }

    /// What will show the client's buffer.
    fn contents(&self, dmabuf: &Dmabuf) -> Result<Contents, String> {
        if !can_import(dmabuf) {
            return Err(format!("{:?} isn't one linear plane", dmabuf.format()));
        }
        let code = dmabuf.format().code;
        let format = hardware_buffer_format(code).ok_or("no such format")?;
        let size = dmabuf.size();
        let (width, height) = (size.w as u32, size.h as u32);
        let stride = dmabuf.strides().next().unwrap_or(0);
        let fd = dmabuf.handles().next().ok_or("no plane")?.as_raw_fd();
        let given = file_size(fd);
        // The client's rows have to be in its buffer.
        let rows_end = stride as i64 * (height as i64 - 1) + width as i64 * 4;
        if (stride as i64) < width as i64 * 4 || given < rows_end {
            return Err(format!(
                "{width}x{height} with rows {stride} bytes apart in {given} bytes"
            ));
        }

        let (template, desc) = self.ndk.allocate(BufferDesc {
            width,
            height,
            layers: 1,
            format,
            usage: USAGE_CPU_READ_RARELY | USAGE_GPU_SAMPLED_IMAGE,
            ..Default::default()
        })?;
        // Gralloc's rows are 4 bytes a pixel in all of FORMATS.
        if desc.stride * 4 == stride {
            if let Ok(buffer) = self.wrap(template, fd, given) {
                return Ok(Contents::Wrapped(buffer));
            }
        } else {
            self.ndk.release(template);
        }

        self.log_once(format!(
            "copying clients' {width}x{height} buffers: their rows are {stride} bytes apart, \
             gralloc's {}",
            desc.stride * 4
        ));
        let (blob, _) = self.ndk.allocate(BufferDesc {
            width: given as u32,
            height: 1,
            layers: 1,
            format: FORMAT_BLOB,
            usage: USAGE_CPU_READ_RARELY | USAGE_GPU_DATA_BUFFER,
            ..Default::default()
        })?;
        let source = self.wrap(blob, fd, given)?;
        let target_format = if is_bgra(code) {
            FORMAT_B8G8R8A8_UNORM
        } else {
            FORMAT_R8G8B8A8_UNORM
        };
        let target = match self.ndk.allocate(BufferDesc {
            width,
            height,
            layers: 1,
            format: target_format,
            usage: USAGE_GPU_SAMPLED_IMAGE | USAGE_GPU_FRAMEBUFFER,
            ..Default::default()
        }) {
            Ok((target, _)) => target,
            Err(error) => {
                self.ndk.release(source);
                return Err(error);
            }
        };
        Ok(Contents::Copied(Copy {
            source,
            target,
            width: width as i32,
            height: height as i32,
            row_length: (stride / 4) as i32,
            size: given,
            format: if is_bgra(code) {
                gl::BGRA_EXT
            } else {
                gl::RGBA
            },
            objects: None,
        }))
    }

    /// A buffer with `template`'s handle and the client's pixels (`fd`, `given` bytes). Releases
    /// the template.
    fn wrap(
        &self,
        template: HardwareBuffer,
        fd: RawFd,
        given: i64,
    ) -> Result<HardwareBuffer, String> {
        let handle = self.handle(template);
        self.ndk.release(template);
        let handle = handle.map_err(|error| format!("no handle: {error}"))?;
        let pixels = handle.fds.first().ok_or("a handle without descriptors")?;
        let needed = file_size(pixels.as_raw_fd());
        if given < needed {
            return Err(format!("a buffer of {given} bytes, gralloc's {needed}"));
        }
        // The client's descriptor in place of the pixels'; the socket hands over a copy of it.
        let mut fds: Vec<RawFd> = handle.fds.iter().map(|fd| fd.as_raw_fd()).collect();
        fds[0] = fd;
        self.rebuild(&handle.data, &fds)
            .map_err(|error| format!("Android didn't take the handle back: {error}"))
    }

    /// The handle of `buffer`.
    fn handle(&self, buffer: HardwareBuffer) -> io::Result<Handle> {
        let (sender, receiver) = socket_pair()?;
        let status = unsafe { (self.ndk.send_handle)(buffer.0, sender.as_raw_fd()) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(-status));
        }
        receive(receiver.as_raw_fd())
    }

    /// A buffer of a handle's data and descriptors.
    fn rebuild(&self, data: &[u8], fds: &[RawFd]) -> io::Result<HardwareBuffer> {
        let (sender, receiver) = socket_pair()?;
        send(sender.as_raw_fd(), data, fds)?;
        let mut buffer = ptr::null_mut();
        let status = unsafe { (self.ndk.receive_handle)(receiver.as_raw_fd(), &mut buffer) };
        if status != 0 || buffer.is_null() {
            return Err(io::Error::from_raw_os_error(-status));
        }
        Ok(HardwareBuffer(buffer))
    }

    fn release(&self, contents: Contents) {
        match contents {
            Contents::Wrapped(buffer) => self.ndk.release(buffer),
            Contents::Copied(copy) => {
                if let (Some((buffer, texture)), Some(Some(gl))) = (copy.objects, self.gl.get()) {
                    unsafe {
                        gl.gles.DeleteTextures(1, &texture);
                        gl.gles.DeleteBuffers(1, &buffer);
                    }
                }
                self.ndk.release(copy.source);
                self.ndk.release(copy.target);
            }
        }
    }

    fn image(
        &self,
        display: ffi::egl::types::EGLDisplay,
        dmabuf: &Dmabuf,
    ) -> Result<EGLImage, String> {
        let contents = self.contents(dmabuf)?;
        let buffer = match &contents {
            Contents::Wrapped(buffer) => *buffer,
            Contents::Copied(copy) => copy.target,
        };
        let attributes = [EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE];
        let image = unsafe {
            let client_buffer = (self.get_native_client_buffer)(buffer.0);
            ffi::egl::CreateImageKHR(
                display,
                ffi::egl::NO_CONTEXT,
                EGL_NATIVE_BUFFER_ANDROID,
                client_buffer as _,
                attributes.as_ptr(),
            )
        };
        if image == ffi::egl::NO_IMAGE_KHR {
            self.release(contents);
            return Err(format!("EGL refused it (0x{:x})", unsafe {
                ffi::egl::GetError()
            }));
        }
        self.images.lock().unwrap().insert(image as usize, contents);
        Ok(image)
    }

    /// GL's functions, if it has GL_EXT_external_buffer. The renderer's context is current.
    fn gl(&self) -> Option<&Gl> {
        self.gl
            .get_or_init(|| unsafe {
                let gles = gl::Gles2::load_with(|name| get_proc_address(name));
                let extensions = gles.GetString(gl::EXTENSIONS);
                let has_external_buffer = !extensions.is_null()
                    && CStr::from_ptr(extensions as *const _)
                        .to_string_lossy()
                        .split(' ')
                        .any(|name| name == "GL_EXT_external_buffer");
                let buffer_storage_external = get_proc_address("glBufferStorageExternalEXT");
                if !has_external_buffer || buffer_storage_external.is_null() {
                    log::warn!("Graphics: GL lacks GL_EXT_external_buffer: buffers gralloc can't describe aren't drawn");
                    return None;
                }
                Some(Gl {
                    gles,
                    buffer_storage_external: mem::transmute(buffer_storage_external),
                })
            })
            .as_ref()
    }

    /// Copy the client's buffer into the image's.
    fn copy(&self, image: EGLImage, copy: &mut Copy) {
        let Some(gl) = self.gl() else {
            return;
        };
        unsafe {
            let gles = &gl.gles;
            let (buffer, texture) = *copy.objects.get_or_insert_with(|| {
                let mut buffer = 0;
                gles.GenBuffers(1, &mut buffer);
                gles.BindBuffer(gl::PIXEL_UNPACK_BUFFER, buffer);
                (gl.buffer_storage_external)(
                    gl::PIXEL_UNPACK_BUFFER,
                    0,
                    copy.size as isize,
                    (self.get_native_client_buffer)(copy.source.0),
                    0,
                );
                let mut texture = 0;
                gles.GenTextures(1, &mut texture);
                gles.BindTexture(gl::TEXTURE_2D, texture);
                gles.EGLImageTargetTexture2DOES(gl::TEXTURE_2D, image);
                (buffer, texture)
            });
            gles.BindBuffer(gl::PIXEL_UNPACK_BUFFER, buffer);
            gles.BindTexture(gl::TEXTURE_2D, texture);
            gles.PixelStorei(gl::UNPACK_ROW_LENGTH, copy.row_length);
            gles.TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                0,
                0,
                copy.width,
                copy.height,
                copy.format,
                gl::UNSIGNED_BYTE,
                ptr::null(),
            );
            gles.PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            gles.BindTexture(gl::TEXTURE_2D, 0);
            gles.BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);
            let error = gles.GetError();
            if error != 0 {
                self.log_once(format!(
                    "copying a client's buffer failed: GL error 0x{error:x}"
                ));
            }
        }
    }
}

impl DmabufImageImporter for Importer {
    fn create_image(
        &self,
        display: ffi::egl::types::EGLDisplay,
        dmabuf: &Dmabuf,
    ) -> Result<EGLImage, EglError> {
        let weak = dmabuf.weak();
        {
            let mut failed = self.failed.lock().unwrap();
            failed.retain(|buffer| !buffer.is_gone());
            if failed.contains(&weak) {
                return Err(EglError::EGLImageCreationFailed);
            }
        }
        self.image(display, dmabuf).map_err(|reason| {
            if self.logged.lock().unwrap().insert(reason.clone()) {
                log::warn!("Graphics: a client's buffer can't be drawn: {reason}");
            }
            self.failed.lock().unwrap().push(weak);
            EglError::EGLImageCreationFailed
        })
    }

    fn image_committed(&self, image: EGLImage) {
        if let Some(Contents::Copied(copy)) = self.images.lock().unwrap().get_mut(&(image as usize))
        {
            self.copy(image, copy);
        }
    }

    fn image_destroyed(&self, image: EGLImage) {
        let contents = self.images.lock().unwrap().remove(&(image as usize));
        if let Some(contents) = contents {
            self.release(contents);
        }
    }
}

/// Make clients' dma-bufs drawable with the renderer's display. Returns whether they are.
pub fn install(display: &EGLDisplay) -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let extensions = display.extensions();
        let missing: Vec<&str> = [
            "EGL_KHR_image_base",
            "EGL_ANDROID_image_native_buffer",
            "EGL_ANDROID_get_native_client_buffer",
        ]
        .into_iter()
        .filter(|name| !extensions.iter().any(|extension| extension == name))
        .collect();
        if !missing.is_empty() {
            log::info!(
                "Graphics: no dma-bufs from clients: EGL lacks {}",
                missing.join(", ")
            );
            return false;
        }
        let ndk = match Ndk::load() {
            Ok(ndk) => ndk,
            Err(error) => {
                log::info!("Graphics: no dma-bufs from clients: {error}");
                return false;
            }
        };
        let get_native_client_buffer =
            unsafe { get_proc_address("eglGetNativeClientBufferANDROID") };
        if get_native_client_buffer.is_null() {
            log::info!("Graphics: no dma-bufs from clients: no eglGetNativeClientBufferANDROID");
            return false;
        }
        let importer = Importer {
            ndk,
            get_native_client_buffer: unsafe { mem::transmute(get_native_client_buffer) },
            gl: OnceLock::new(),
            images: Mutex::new(HashMap::new()),
            failed: Mutex::new(Vec::new()),
            logged: Mutex::new(HashSet::new()),
        };
        set_dmabuf_image_importer(Box::new(importer))
    })
}

fn file_size(fd: RawFd) -> i64 {
    unsafe { libc::lseek(fd, 0, libc::SEEK_END) }
}

fn socket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn control_space() -> usize {
    unsafe { libc::CMSG_SPACE((mem::size_of::<RawFd>() * MAX_HANDLE_FDS) as u32) as usize }
}

/// One message from a socket, with its descriptors.
fn receive(socket: RawFd) -> io::Result<Handle> {
    let mut data = vec![0u8; 4096];
    let mut control = vec![0u8; control_space()];
    let mut io = libc::iovec {
        iov_base: data.as_mut_ptr() as *mut c_void,
        iov_len: data.len(),
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut io;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut c_void;
    message.msg_controllen = control.len() as _;
    let length = unsafe { libc::recvmsg(socket, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / mem::size_of::<RawFd>();
                let first = libc::CMSG_DATA(header) as *const RawFd;
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(ptr::read_unaligned(first.add(i))));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::other("the handle didn't fit"));
    }
    data.truncate(length as usize);
    Ok(Handle { data, fds })
}

/// Send a message with descriptors.
fn send(socket: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut control = vec![0u8; control_space()];
    let mut io = libc::iovec {
        iov_base: data.as_ptr() as *mut c_void,
        iov_len: data.len(),
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut io;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut c_void;
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE((mem::size_of::<RawFd>() * fds.len()) as u32) } as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((mem::size_of::<RawFd>() * fds.len()) as u32) as _;
        ptr::copy_nonoverlapping(
            fds.as_ptr(),
            libc::CMSG_DATA(header) as *mut RawFd,
            fds.len(),
        );
    }
    if unsafe { libc::sendmsg(socket, &message, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
