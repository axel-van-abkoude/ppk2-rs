use anyhow::Result;
use clap::Parser;
use csv::{Reader, Writer};
use minifb::{Key, KeyRepeat, Window, WindowOptions};
use plotters::prelude::*;
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
use std::error::Error;
use std::{
    path::Path,
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
const FPS: usize = 200;
// Move with 1/SCALE of distance in plot
const SCALE: f32 = 10.0;

struct PlotBounds {
    right: f32,
    left: f32,
    top: f32,
    bot: f32,
    max_right: f32,
    min_left: f32,
    max_top: f32,
    min_bot: f32,
}

impl PlotBounds {

    /// Infer values of PlotBound from csv file
    pub fn from_file<T: AsRef<Path>>(file: T) -> Result<PlotBounds> {
        let mut max_right = f32::MIN;
        let mut min_left = f32::MAX;
        let mut max_top = f32::MIN;
        let mut min_bot = f32::MAX;

        let mut rdr = Reader::from_path(file).unwrap();
        for msample in rdr.deserialize::<Sample>() {
            let sample = msample?;
            // Update max and min timestamp
            if sample.timestamp > max_right {
                max_right = sample.timestamp;
            }
            if sample.timestamp < min_left {
                min_left = sample.timestamp;
            }
            // Update max and min power
            if sample.power > max_top {
                max_top = sample.power;
            }
            if sample.power < min_bot {
                min_bot = sample.power;
            }
        }
        let margin = max_right/SCALE;
        Ok(PlotBounds {
            right: max_right,
            left: min_left,
            top: max_top,
            bot: min_bot,
            max_right: max_right+margin,
            min_left: min_left-margin,
            max_top: max_top+margin,
            min_bot: min_bot-margin,
        })
    }

    fn zoom_out(&mut self) {
        let scalex = (self.right - self.left) / SCALE;
        let scaley = (self.top - self.bot) / SCALE;
        self.right += scalex;
        self.left -= scalex;
        // self.top += scaley;
        // self.bot -= scaley;
    }

    fn zoom_in(&mut self) {
        let scalex = (self.right - self.left) / SCALE;
        let scaley = (self.top - self.bot) / SCALE;
        self.right -= scalex;
        self.left += scalex;
        // self.top -= scaley;
        // self.bot += scaley;
    }

    fn move_right(&mut self) {
        let scale = (self.right - self.left) / SCALE;
        let ret = self.right + scale;
        if ret < self.max_right {
            self.right = ret;
            self.left = self.left + scale;
        }
    }

    fn move_left(&mut self) {
        let scale = (self.right - self.left) / SCALE;
        let ret = self.left - scale;
        if ret > self.min_left {
            self.left = ret;
            self.right = self.right - scale;
        }
    }

    fn move_up(&mut self) {
        let scale = (self.top - self.bot) / SCALE;
        let ret = self.top + scale;
        if ret < self.max_top {
            self.top = ret;
            self.bot = self.bot + scale;
        }
    }

    fn move_down(&mut self) {
        let scale = (self.top - self.bot) / SCALE;
        let ret = self.bot - scale;
        if ret > self.min_bot {
            self.bot = ret;
            self.top = self.top - scale;
        }
    }
}

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

fn update_bounds(keys: Vec<Key>, bound: &mut PlotBounds) {
    for key in keys {
        match key {
            Key::Minus => bound.zoom_out(),
            Key::Equal => bound.zoom_in(),
            Key::W | Key::Up => bound.move_up(),
            Key::A | Key::Left => bound.move_left(),
            Key::S | Key::Down => bound.move_down(),
            Key::D | Key::Right => bound.move_right(),
            _ => continue,
        }
    }
}

fn draw_buffer(
    args: &Args,
    buf: &mut [u8],
    bound: &mut PlotBounds,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = BitMapBackend::<BGRXPixel>::with_buffer_and_format(buf, (W as u32, H as u32))?
        .into_drawing_area();

    root.fill(&BLACK)?;

    let mut chart = ChartBuilder::on(&root)
        .margin(10)
        .set_all_label_area_size(30)
        .build_cartesian_2d((*bound).left..(*bound).right, (*bound).bot..(*bound).top)?;

    chart
        .configure_mesh()
        .label_style(("sans-serif", 15).into_font().color(&WHITE))
        .axis_style(&WHITE)
        .draw()?;

    chart
        .configure_mesh()
        .bold_line_style(&WHITE.mix(0.2))
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
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &GREEN));
    chart.plotting_area().present()?;
    Ok(())
}

fn start_plot(args: &Args) -> Result<(), Box<dyn Error>> {
    let mut buf = BufferWrapper(vec![0u32; W * H]);

    let mut bound = PlotBounds::from_file(args.file.clone())?;

    let mut window = Window::new("Title", W, H, WindowOptions::default())?;
    window.set_target_fps(FPS);

    draw_buffer(args, buf.borrow_mut(), &mut bound)?;
    window.update_with_buffer(buf.borrow(), W, H)?;

    while window.is_open() && !window.is_key_pressed(Key::Escape, KeyRepeat::No) {
        window.update_with_buffer(buf.borrow(), W, H)?;
        let keys = window.get_keys_pressed(KeyRepeat::Yes);
        if keys.is_empty() {
            continue;
        }

        update_bounds(keys, &mut bound);

        draw_buffer(args, buf.borrow_mut(), &mut bound)?;
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
