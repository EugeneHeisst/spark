//! Local level detection: no audio leaves this graph and nothing is played back.
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{
    AnalyserNode, AudioContext, MediaStream, MediaStreamAudioSourceNode, MediaStreamTrack,
};

struct Meter {
    source: MediaStreamAudioSourceNode,
    analyser: AnalyserNode,
    track: MediaStreamTrack,
    samples: Vec<f32>,
    last_voice: f64,
    speaking: bool,
}
impl Drop for Meter {
    fn drop(&mut self) {
        let _ = self.source.disconnect();
        let _ = self.analyser.disconnect();
    }
}
struct State {
    context: Option<AudioContext>,
    meters: HashMap<String, Meter>,
}
pub struct VoiceActivity {
    state: Rc<RefCell<State>>,
    callback: Rc<dyn Fn(String, bool)>,
    timer: RefCell<Option<i32>>,
}
impl VoiceActivity {
    pub fn new(callback: Rc<dyn Fn(String, bool)>) -> Self {
        Self {
            state: Rc::new(RefCell::new(State {
                context: None,
                meters: HashMap::new(),
            })),
            callback,
            timer: RefCell::new(None),
        }
    }
    pub fn add(&self, peer: &str, stream: &MediaStream) -> Result<(), JsValue> {
        let Some(track) = stream
            .get_audio_tracks()
            .get(0)
            .dyn_into::<MediaStreamTrack>()
            .ok()
        else {
            return Ok(());
        };
        self.remove(peer);
        let mut state = self.state.borrow_mut();
        if state.context.is_none() {
            state.context = Some(AudioContext::new()?);
        }
        let context = state.context.as_ref().unwrap();
        let _ = context.resume();
        let source = context.create_media_stream_source(stream)?;
        let analyser = context.create_analyser()?;
        analyser.set_fft_size(1024);
        source.connect_with_audio_node(&analyser)?;
        state.meters.insert(
            peer.into(),
            Meter {
                source,
                analyser,
                track,
                samples: vec![0.; 1024],
                last_voice: -1000.,
                speaking: false,
            },
        );
        drop(state);
        if self.timer.borrow().is_none() {
            let weak = Rc::downgrade(&self.state);
            let callback = self.callback.clone();
            let tick = Closure::<dyn FnMut()>::new(move || {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let now = js_sys::Date::now();
                let changes = {
                    let mut state = state.borrow_mut();
                    state
                        .meters
                        .iter_mut()
                        .filter_map(|(peer, meter)| {
                            meter
                                .analyser
                                .get_float_time_domain_data(&mut meter.samples);
                            let rms = (meter.samples.iter().map(|x| x * x).sum::<f32>()
                                / meter.samples.len() as f32)
                                .sqrt();
                            let enabled = meter.track.enabled()
                                && !meter.track.muted()
                                && meter.track.ready_state()
                                    == web_sys::MediaStreamTrackState::Live;
                            if enabled && rms > 0.008 {
                                meter.last_voice = now;
                            }
                            let speaking = enabled && now - meter.last_voice < 240.;
                            if speaking == meter.speaking {
                                return None;
                            }
                            meter.speaking = speaking;
                            Some((peer.clone(), speaking))
                        })
                        .collect::<Vec<_>>()
                };
                for (peer, speaking) in changes {
                    callback(peer, speaking);
                }
            })
            .into_js_value();
            let timer = web_sys::window()
                .unwrap()
                .set_interval_with_callback_and_timeout_and_arguments_0(tick.unchecked_ref(), 80)?;
            *self.timer.borrow_mut() = Some(timer);
        }
        Ok(())
    }
    pub fn remove(&self, peer: &str) {
        let removed = self.state.borrow_mut().meters.remove(peer);
        if removed.is_some() {
            (self.callback)(peer.into(), false);
        }
    }
    pub fn clear(&self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            web_sys::window().unwrap().clear_interval_with_handle(timer);
        }
        let (meters, context) = {
            let mut state = self.state.borrow_mut();
            (std::mem::take(&mut state.meters), state.context.take())
        };
        for peer in meters.keys() {
            (self.callback)(peer.clone(), false);
        }
        drop(meters);
        if let Some(context) = context {
            let _ = context.close();
        }
    }
}
impl Drop for VoiceActivity {
    fn drop(&mut self) {
        self.clear();
    }
}
