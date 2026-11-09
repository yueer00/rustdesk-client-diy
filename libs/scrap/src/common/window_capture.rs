#[cfg(feature = "vram")]
use crate::AdapterDevice;
use crate::{Frame, PixelBuffer, TraitCapturer};
use std::{
    error::Error,
    io::{self, ErrorKind},
    sync::mpsc::{self, Receiver, SyncSender},
    time::Duration,
};
use windows_capture::{
    capture::{CaptureControl, GraphicsCaptureApiHandler},
    frame::Frame as CapturedFrame,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};

type CaptureError = Box<dyn Error + Send + Sync>;

pub fn is_supported() -> bool {
    GraphicsCaptureApi::is_supported().unwrap_or(false)
        && GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false)
}

struct Pixels {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

struct CaptureHandler(SyncSender<Pixels>);

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = SyncSender<Pixels>;
    type Error = CaptureError;

    fn new(sender: Self::Flags) -> Result<Self, Self::Error> {
        Ok(Self(sender))
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut CapturedFrame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width() as usize;
        let height = frame.height() as usize;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let mut buffer = frame.buffer()?;
        let data = buffer.as_raw_nopadding_buffer()?.to_vec();
        match self.0.try_send(Pixels {
            width,
            height,
            data,
        }) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
            Err(mpsc::TrySendError::Disconnected(_)) => control.stop(),
        }
        Ok(())
    }
}

pub struct WindowCapturer {
    control: Option<CaptureControl<CaptureHandler, CaptureError>>,
    receiver: Receiver<Pixels>,
    pixels: Pixels,
    first_frame: bool,
}

impl WindowCapturer {
    pub fn new(hwnd: usize, timeout: Duration) -> io::Result<Self> {
        let window = Window::from_raw_hwnd(hwnd as *mut _);
        let (sender, receiver) = mpsc::sync_channel(1);
        let settings = Settings::new(
            window,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
            ColorFormat::Bgra8,
            sender,
        );
        let control = CaptureHandler::start_free_threaded(settings)
            .map_err(|e| io::Error::new(ErrorKind::Other, e.to_string()))?;
        let pixels = match receiver.recv_timeout(timeout) {
            Ok(pixels) => pixels,
            Err(e) => {
                if let Err(stop_error) = control.stop() {
                    hbb_common::throttled_log!(
                        Duration::from_secs(30),
                        warn,
                        "Failed to stop window capture: {stop_error}"
                    );
                }
                let kind = match e {
                    mpsc::RecvTimeoutError::Timeout => ErrorKind::TimedOut,
                    mpsc::RecvTimeoutError::Disconnected => ErrorKind::BrokenPipe,
                };
                return Err(io::Error::new(kind, e));
            }
        };
        Ok(Self {
            control: Some(control),
            receiver,
            pixels,
            first_frame: true,
        })
    }

    pub fn width(&self) -> usize {
        self.pixels.width
    }
    pub fn height(&self) -> usize {
        self.pixels.height
    }
}

impl TraitCapturer for WindowCapturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        if self.first_frame {
            self.first_frame = false;
        } else {
            match self.receiver.recv_timeout(timeout) {
                Ok(pixels) => self.pixels = pixels,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.control.as_ref().map_or(true, |c| c.is_finished()) {
                        return Err(ErrorKind::BrokenPipe.into());
                    }
                    return Err(ErrorKind::WouldBlock.into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ErrorKind::BrokenPipe.into());
                }
            }
        }
        Ok(Frame::PixelBuffer(PixelBuffer::with_BGRA(
            &self.pixels.data,
            self.pixels.width,
            self.pixels.height,
        )))
    }

    fn is_gdi(&self) -> bool {
        false
    }
    fn set_gdi(&mut self) -> bool {
        false
    }

    #[cfg(feature = "vram")]
    fn device(&self) -> AdapterDevice {
        AdapterDevice::default()
    }
    #[cfg(feature = "vram")]
    fn set_output_texture(&mut self, _texture: bool) {}
}

impl Drop for WindowCapturer {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            if let Err(e) = control.stop() {
                hbb_common::throttled_log!(
                    Duration::from_secs(30),
                    warn,
                    "Failed to stop window capture: {e}"
                );
            }
        }
    }
}
