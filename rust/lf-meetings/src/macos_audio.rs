use crate::{
    system_audio::{
        AudioCallback, AudioSource, StreamingResampler, SystemAudioUnavailable, SCK_RATES,
    },
    watcher::MicUsageProbe,
};
use block2::RcBlock;
use objc2::{
    define_class, msg_send, rc::Retained, runtime::ProtocolObject, AnyThread, DefinedClass,
};
use objc2_core_audio::{
    kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectGetPropertyData,
    AudioObjectPropertyAddress,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};
use std::{
    ptr::{self, NonNull},
    sync::{mpsc, Mutex},
    time::Duration,
};

pub struct CoreAudioMicUsageProbe;
// Python's _RUNNING_SOMEWHERE uses 'goin' (the SDK names it DeviceIsRunning).
const RUNNING_SOMEWHERE: u32 = u32::from_be_bytes(*b"goin");
impl CoreAudioMicUsageProbe {
    fn get_u32(object: u32, selector: u32) -> anyhow::Result<u32> {
        let mut address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut value: u32 = 0;
        let mut size = std::mem::size_of_val(&value) as u32;
        // All pointers refer to live, correctly sized property storage.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&mut address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        };
        anyhow::ensure!(status == 0, "AudioObjectGetPropertyData failed: {status}");
        Ok(value)
    }
}
impl MicUsageProbe for CoreAudioMicUsageProbe {
    fn mic_in_use(&self) -> anyhow::Result<bool> {
        let device = Self::get_u32(
            kAudioObjectSystemObject as u32,
            kAudioHardwarePropertyDefaultInputDevice,
        )?;
        Ok(device != 0 && Self::get_u32(device, RUNNING_SOMEWHERE)? != 0)
    }
}

struct OutputIvars {
    callback: AudioCallback,
    resampler: Mutex<StreamingResampler>,
}
define_class!(
    #[unsafe(super = NSObject)]
    #[ivars = OutputIvars]
    struct LocalflowSystemAudioOutput;
    unsafe impl NSObjectProtocol for LocalflowSystemAudioOutput {}
    unsafe impl SCStreamOutput for LocalflowSystemAudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Audio {
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // The delegate receives a live CMSampleBuffer for this invocation.
                let Some(block) = (unsafe { sample.data_buffer() }) else {
                    return;
                };
                let length = unsafe { block.data_length() };
                if length == 0 || !length.is_multiple_of(std::mem::size_of::<f32>()) {
                    return;
                }
                let mut audio = vec![0.0f32; length / std::mem::size_of::<f32>()];
                // Audio is mono native-endian float32, and the destination owns length bytes.
                if unsafe {
                    block.copy_data_bytes(
                        0,
                        length,
                        NonNull::new(audio.as_mut_ptr()).unwrap().cast(),
                    )
                } != 0
                {
                    return;
                }
                let ivars = self.ivars();
                let audio = ivars
                    .resampler
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .process(&audio);
                (ivars.callback)(audio);
            }));
            if result.is_err() {
                log::warn!("system audio buffer conversion failed");
            }
        }
    }
);
impl LocalflowSystemAudioOutput {
    fn new(callback: AudioCallback, capture_rate: u32, sample_rate: u32) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            callback,
            resampler: Mutex::new(StreamingResampler::new(capture_rate, sample_rate)),
        });
        // NSObject has no additional initialization requirements.
        unsafe { msg_send![super(this), init] }
    }
}
struct ShareableContent(Retained<SCShareableContent>);
// SCShareableContent is an immutable snapshot delivered on ScreenCaptureKit's callback queue.
unsafe impl Send for ShareableContent {}

fn error_text(error: *mut NSError) -> Option<String> {
    // Completion handlers provide either nil or a valid NSError during the call.
    unsafe { error.as_ref() }.map(|e| e.to_string())
}
fn wait<T>(
    rx: mpsc::Receiver<T>,
    timeout: Duration,
    what: &str,
) -> Result<T, SystemAudioUnavailable> {
    rx.recv_timeout(timeout)
        .map_err(|_| SystemAudioUnavailable(format!("ScreenCaptureKit: {what} timed out")))
}
pub struct ScreenCaptureKitSource {
    pub sample_rate: u32,
    pub capture_rate: u32,
    pub timeout: Duration,
    stream: Option<Retained<SCStream>>,
    output: Option<Retained<LocalflowSystemAudioOutput>>,
}
impl ScreenCaptureKitSource {
    pub fn new(sample_rate: u32) -> Self {
        assert!(sample_rate > 0, "sample rate must be positive");
        Self {
            sample_rate,
            capture_rate: if SCK_RATES.contains(&sample_rate) {
                sample_rate
            } else {
                48000
            },
            timeout: Duration::from_secs(10),
            stream: None,
            output: None,
        }
    }
}
impl AudioSource for ScreenCaptureKitSource {
    fn label(&self) -> &str {
        "ScreenCaptureKit"
    }
    fn start(&mut self, callback: AudioCallback) -> Result<(), SystemAudioUnavailable> {
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // Retain the immutable content before the completion handler returns.
                let content = unsafe { Retained::retain(content) }.map(ShareableContent);
                let _ = tx.send((content, error_text(error)));
            },
        );
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
        let (content, error) = wait(rx, self.timeout, "listing shareable content")?;
        let no_display = || {
            SystemAudioUnavailable(format!("ScreenCaptureKit: no display available — grant Screen Recording permission to this Python binary ({})", error.as_deref().unwrap_or("None")))
        };
        if error.is_some() {
            return Err(no_display());
        }
        let content = content.ok_or_else(no_display)?;
        let displays = unsafe { content.0.displays() };
        if displays.is_empty() {
            return Err(no_display());
        }
        let display = displays.objectAtIndex(0);
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(self.capture_rate as _);
            config.setChannelCount(1);
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
        }
        let output = LocalflowSystemAudioOutput::new(callback, self.capture_rate, self.sample_rate);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Audio,
                None,
            )
        }
        .map_err(|e| {
            SystemAudioUnavailable(format!("ScreenCaptureKit: addStreamOutput failed ({e})"))
        })?;
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send(error_text(error));
        });
        self.stream = Some(stream);
        self.output = Some(output);
        unsafe {
            self.stream
                .as_ref()
                .unwrap()
                .startCaptureWithCompletionHandler(Some(&handler))
        };
        match wait(rx, self.timeout, "start")? {
            Some(error) => Err(SystemAudioUnavailable(format!(
                "ScreenCaptureKit: start failed ({error})"
            ))),
            None => Ok(()),
        }
    }
    fn stop(&mut self) -> Result<(), SystemAudioUnavailable> {
        if let Some(stream) = self.stream.take() {
            let (tx, rx) = mpsc::channel();
            let handler = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(error_text(error));
            });
            unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
            if wait(rx, self.timeout, "stop").is_err() {
                log::warn!("ScreenCaptureKit: stop timed out");
            }
        }
        self.output = None;
        Ok(())
    }
}
impl Drop for ScreenCaptureKitSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_rates_and_async_timeout_without_devices() {
        for rate in SCK_RATES {
            let source = ScreenCaptureKitSource::new(*rate);
            assert_eq!(source.capture_rate, *rate);
            assert_eq!(source.timeout, Duration::from_secs(10));
            assert_eq!(source.label(), "ScreenCaptureKit");
        }
        assert_eq!(ScreenCaptureKitSource::new(44100).capture_rate, 48000);
        let (tx, rx) = mpsc::channel();
        tx.send(42).unwrap();
        assert_eq!(wait(rx, Duration::ZERO, "start").unwrap(), 42);
        let (_tx, rx) = mpsc::channel::<()>();
        assert_eq!(
            wait(rx, Duration::ZERO, "start").unwrap_err().to_string(),
            "ScreenCaptureKit: start timed out"
        );
        assert_eq!(RUNNING_SOMEWHERE, u32::from_be_bytes(*b"goin"));
    }
}
