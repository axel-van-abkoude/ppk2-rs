use anyhow::Result;
use clap::Parser;
use csv::{Reader, Writer};
use minifb::{Key, KeyRepeat, Window, WindowOptions};
use plotters::prelude::*;
use plotters::{coord::ranged1d::AsRangedCoord, prelude::*};
use plotters_bitmap::bitmap_pixel::BGRXPixel;
use plotters_bitmap::BitMapBackend;
use ppk2::{
    measurement::MeasurementMatch,
    try_find_ppk2_port,
    types::{DevicePower, LogicPortPins, MeasurementMode, SourceVoltage},
    Ppk2,
};
use serde::{Deserialize, Serialize};
use std::borrow::{Borrow, BorrowMut};
use std::collections::VecDeque;
use std::error::Error;
use std::thread;
use std::time::SystemTime;
use std::{
    sync::mpsc::RecvTimeoutError,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, Level as LogLevel};

#[derive(Parser)]
struct Args {
    #[clap(
        env,
        short = 'p',
        long,
        help = "The serial port the PPK2 is connected to. If unspecified, will try to find the PPK2 automatically"
    )]
    serial_port: Option<String>,

    #[clap(
        env,
        short = 'v',
        long,
        help = "The voltage of the device source in mV",
        default_value = "0"
    )]
    voltage: SourceVoltage,

    #[clap(
        env,
        short = 'e',
        long,
        help = "Enable power",
        default_value = "disabled"
    )]
    power: DevicePower,

    #[clap(
        env,
        short = 'm',
        long,
        help = "Measurement mode",
        default_value = "source"
    )]
    mode: MeasurementMode,

    #[clap(env, short = 'l', long, help = "The log level", default_value = "info")]
    log_level: LogLevel,

    #[clap(
        env,
        short = 's',
        long,
        help = "The maximum number of samples to be taken per second. Uses averaging of device samples Samples are analyzed in chunks, and as such the actual number of samples per second will deviate",
        default_value = "100"
    )]
    sps: usize,
    #[clap(
        env,
        short = 'f',
        long,
        help = "The csv file the data will be written to, when no file is specified the program does nothing"
    )]
    file: String,
}

const W: usize = 1000;
const H: usize = 800;
const STEP: f32 = 1_000.0;
const FPS: usize = 30;

#[derive(Clone)]
struct BufferWrapper(Vec<u32>);
impl Borrow<[u8]> for BufferWrapper {
    fn borrow(&self) -> &[u8] {
        // Safe for alignment: align_of(u8) <= align_of(u32)
        // Safe for cast: u32 can be thought of as being transparent over [u8; 4]
        unsafe { std::slice::from_raw_parts(self.0.as_ptr() as *const u8, self.0.len() * 4) }
    }
}
impl BorrowMut<[u8]> for BufferWrapper {
    fn borrow_mut(&mut self) -> &mut [u8] {
        // Safe for alignment: align_of(u8) <= align_of(u32)
        // Safe for cast: u32 can be thought of as being transparent over [u8; 4]
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, self.0.len() * 4) }
    }
}
impl Borrow<[u32]> for BufferWrapper {
    fn borrow(&self) -> &[u32] {
        self.0.as_slice()
    }
}
impl BorrowMut<[u32]> for BufferWrapper {
    fn borrow_mut(&mut self) -> &mut [u32] {
        self.0.as_mut_slice()
    }
}

fn handle_keys(
    keys: Vec<Key>,
    bound_right: &mut f32,
    bound_left: &mut f32,
    bound_top: &mut f32,
    bound_bot: &mut f32,
) {
    for key in keys {
        match key {
            Key::Escape => {
                break;
            }
            Key::Minus => {
                *bound_right += STEP;
                *bound_left += STEP;
                *bound_top += STEP;
                *bound_bot += STEP;
            }
            Key::Equal => {
                *bound_right -= STEP;
                *bound_left -= STEP;
                *bound_top -= STEP;
                *bound_bot -= STEP;
            }
            Key::A => {
                *bound_right -= STEP;
                *bound_left += STEP;
            }
            Key::D => {
                *bound_right += STEP;
                *bound_left -= STEP;
            }
            Key::S => {
                *bound_top -= STEP;
                *bound_bot += STEP;
            }
            Key::W => {
                *bound_top += STEP;
                *bound_bot -= STEP;
            }
            _ => {
                continue;
            }
        }
    }
}

fn start_plot(args: &Args) -> Result<(), Box<dyn Error>> {
    let mut buf = BufferWrapper(vec![0u32; W * H]);

    let mut bound_right: f32 = 100_000.0;
    let mut bound_left: f32 = 0.0;
    let mut bound_top: f32 = 1_000.0;
    let mut bound_bot: f32 = 0.0;

    let mut window = Window::new("Title", W, H, WindowOptions::default())?;
    window.set_target_fps(FPS);
    window.update_with_buffer(buf.borrow(), W, H)?;

    while window.is_open() && !window.is_key_down(Key::Escape){

        window.update_with_buffer(buf.borrow(), W, H)?;
        let keys = window.get_keys_pressed(KeyRepeat::Yes);
        if keys.is_empty() {
            continue;
        }

        handle_keys(keys, &mut bound_right, &mut bound_left, &mut bound_top, &mut bound_bot);

        {
            let root = BitMapBackend::<BGRXPixel>::with_buffer_and_format(
                buf.borrow_mut(),
                (W as u32, H as u32),
            )?
            .into_drawing_area();

            root.fill(&BLACK)?;

            let mut chart = ChartBuilder::on(&root)
                .margin(10)
                .set_all_label_area_size(30)
                .build_cartesian_2d(-bound_left..bound_right, -bound_bot..bound_top)?;

            chart
                .configure_mesh()
                .label_style(("sans-serif", 15).into_font().color(&GREEN))
                .axis_style(&GREEN)
                .draw()?;

            chart
                .configure_mesh()
                .bold_line_style(&GREEN.mix(0.2))
                .light_line_style(&TRANSPARENT)
                .draw()?;

            let mut rdr = Reader::from_path(args.file.clone()).unwrap();
            chart
                .draw_series(LineSeries::new(
                    rdr.deserialize::<Sample>().map(|xs| match xs {
                        Ok(s) => (s.timestamp, s.power),
                        Err(_) => {
                            todo!()
                        }
                    }),
                    &GREEN,
                ))
                .unwrap()
                .label("trace")
                .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLUE));
            chart.plotting_area().present()?;
        }
    }
    Ok(())
}

fn configure_ppk2(args: &Args) -> Result<Ppk2> {
    let ppk2_port = match &args.serial_port {
        Some(p) => p,
        None => &try_find_ppk2_port()?,
    };

    // Connect to PPK2 and initialize
    let mut ppk2 = Ppk2::new(ppk2_port, args.mode)?;
    ppk2.set_source_voltage(args.voltage)?;
    ppk2.set_device_power(args.power)?;

    Ok(ppk2)
}

#[derive(Debug, Deserialize, Serialize)]
struct Sample {
    #[serde(rename = "timestamp (μs)")]
    timestamp: f32,
    #[serde(rename = "power (μA)")]
    power: f32,
    #[serde(rename = "pins (D0-D7)")]
    pins: LogicPortPins,
}

fn write_to_file(ppk2: Ppk2, args: &Args, duration: Duration) -> Result<Ppk2> {
    let mut wtr = Writer::from_path(args.file.clone())?;
    let (rx, kill) = ppk2.start_measurement(args.sps)?;

    info!(
        "Started measurement of {} seconds to \'{:}\'",
        duration.as_secs(),
        args.file
    );

    let start = Instant::now();

    loop {
        let now = Instant::now().duration_since(start);
        if now > duration {
            break Ok(kill()?);
        }

        use MeasurementMatch::*;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Match(m)) => {
                wtr.serialize(Sample {
                    timestamp: now.as_secs_f32(),
                    power: m.micro_amps,
                    pins: m.pins,
                })?;
            }
            Ok(NoMatch) => {
                debug!("No match in the last chunk of measurements");
            }
            Err(RecvTimeoutError::Disconnected) => break Ok(kill()?),
            Err(e) => {
                error!("Error receiving data: {e:?}");
                break Err(e)?;
            }
        }
    }
}

fn wait_for_power(ppk2: Ppk2) -> Result<Ppk2> {
    let (rx, kill) = ppk2.start_measurement(100)?;
    info!("Waiting for power...");

    loop {
        use MeasurementMatch::*;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Match(m)) if m.micro_amps > 0.0 => {
                info!("Power detected!");
                break Ok(kill()?);
            }
            Ok(_) => continue,
            Err(e) => {
                error!("Error receiving data: {e:?}");
                break Err(e)?;
            }
        }
    }
}

fn main() -> Result<()> {
    let args = &Args::parse();

    // let subscriber = FmtSubscriber::builder()
    //     .with_max_level(args.log_level)
    //     .finish();
    // tracing::subscriber::set_global_default(subscriber)?;

    // let mut ppk2 = configure_ppk2(args).expect("Something went wrong configuring the ppk2:\n");

    // ppk2 = wait_for_power(ppk2).expect("Something went wrong waiting for power");
    // ppk2 = write_to_file(
    //     ppk2,
    //     args,
    //     Duration::from_secs(5),
    // )
    // .expect("Something went wrong in measurement:\n");

    start_plot(args).unwrap();

    info!("Goodbye!");
    Ok(())
}
