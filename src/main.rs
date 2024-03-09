#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use core::cell::RefCell;
use core::fmt::Write;

use display_interface_spi::SPIInterfaceNoCS;
use eg_seven_segment::SevenSegmentStyleBuilder;
use embassy_embedded_hal::shared_bus::blocking;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Duration, Ticker};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::Drawable;
use embedded_graphics::geometry::{Point, Size};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::{Rgb565, RgbColor};
use embedded_graphics::prelude::Primitive;
use embedded_graphics::primitives::{PrimitiveStyleBuilder, Rectangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyle, TextStyleBuilder};
use embedded_graphics::text::renderer::TextRenderer;
use embedded_hal_async::digital::Wait;
use enum_iterator::Sequence;
use esp32s2_hal::{clock::ClockControl, embassy, IO, peripherals::Peripherals, prelude::*, psram};
use esp32s2_hal::clock::Clocks;
use esp32s2_hal::gpio::{GpioPin, Unknown};
use esp32s2_hal::i2c::I2C;
use esp32s2_hal::peripherals::I2C0;
use esp32s2_hal::spi::master::Spi;
use esp32s2_hal::spi::SpiMode;
use esp32s2_hal::timer::TimerGroup;
use esp_backtrace;
use esp_println::println;
use heapless::String;
use ina219_rs::ina219::{INA219, PowerMonitor};
use profont::PROFONT_24_POINT;
use st7789::{Orientation, ST7789};
use static_cell::{make_static, StaticCell};

use crate::graphics::GraphicUtils;

mod graphics;

const ROWSTART: i32 = 40;
const COLSTART: i32 = 54;

#[derive(Debug, Clone)]
struct InputData {
    button: u8,
    is_low: bool,
    is_high: bool,
    power: PowerMonitor,
}

impl InputData {
    fn new() -> Self {
        InputData {
            button: 0,
            is_low: false,
            is_high: false,
            power: PowerMonitor {
                Shunt: 0.0,
                Voltage: 0.0,
                Current: 0.0,
                Power: 0.0,
            },
        }
    }
}

static INPUT_CHANNEL: embassy_sync::channel::Channel<CriticalSectionRawMutex, InputData, 1> = embassy_sync::channel::Channel::new();

#[global_allocator]
static ALLOCATOR: esp_alloc::EspHeap = esp_alloc::EspHeap::empty();

#[derive(Debug, PartialEq, Sequence)]
enum PowerDisplay {
    Shunt,
    Voltage,
    Current,
    Power,
}

fn init_psram_heap() {
    unsafe {
        ALLOCATOR.init(psram::psram_vaddr_start() as *mut u8, psram::PSRAM_BYTES);
    }
}

macro_rules! singleton {
    ($val:expr, $typ:ty) => {{
        static STATIC_CELL: StaticCell<$typ> = StaticCell::new();
        STATIC_CELL.init($val)
    }};
}

fn display_text<D, S>(display: &mut D, pos: Point, character_style: S,
                      text_style: TextStyle, text: &str) where D: DrawTarget<Color=Rgb565>, S: TextRenderer<Color=Rgb565> {
    let _ = Text::with_text_style(
        text,
        pos,
        character_style,
        text_style,
    )
        .draw(display);
}

fn create_point_from(point: Point) -> Point {
    create_point(point.x, point.y)
}

fn create_point(x: i32, y: i32) -> Point {
    Point::new(x + ROWSTART, y + COLSTART)
}

#[embassy_executor::task]
pub async fn handle_button_d1(pin: GpioPin<Unknown, 1>) {
    let mut button = pin.into_pull_down_input();
    loop {
        button.wait_for_any_edge().await.unwrap();
        let mut input_data = InputData::new();
        input_data.button = 1;
        input_data.is_high = button.is_high().unwrap();
        input_data.is_low = button.is_low().unwrap();
        INPUT_CHANNEL.send(input_data).await;
    }
}

#[embassy_executor::task]
pub async fn handle_button_d2(pin: GpioPin<Unknown, 2>) {
    let mut button = pin.into_pull_down_input();
    loop {
        button.wait_for_any_edge().await.unwrap();
        let mut input_data = InputData::new();
        input_data.button = 2;
        input_data.is_high = button.is_high().unwrap();
        input_data.is_low = button.is_low().unwrap();
        INPUT_CHANNEL.send(input_data).await;
    }
}

#[embassy_executor::task]
pub async fn handle_power(i2c: blocking::i2c::I2cDevice<'static, CriticalSectionRawMutex, I2C<'static, I2C0>>) {
    let mut ina219 = INA219::new(i2c);
    match ina219.init() {
        Err(e) => {
            println!("{:?}", e);
            return;
        }
        _ => {}
    }

    // let mut i = 0.0;
    let mut ticker = Ticker::every(Duration::from_millis(500));
    loop {
        if let Ok(power_monitor) = ina219.sense() {
            let mut input_data = InputData::new();
            // let mut power_monitor = PowerMonitor::new(i, i, i, i);
            input_data.power = power_monitor;
            INPUT_CHANNEL.send(input_data).await;
        }
        // i += 1.0;
        ticker.next().await;
    }
}

#[main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = Peripherals::take();

    init_psram_heap();

    let system = peripherals.SYSTEM.split();

    let clocks = singleton!(
        ClockControl::max(system.clock_control).freeze(),
        Clocks
    );

    let timer_group0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    embassy::init(&clocks, timer_group0);

    esp_println::logger::init_logger_from_env();

    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);

    // enable i2c_power
    let i2c_power = io.pins.gpio7;
    i2c_power.into_push_pull_output().set_high().unwrap();

    let mut i2c0 = I2C::new(
        peripherals.I2C0,
        io.pins.gpio3,
        io.pins.gpio4,
        100u32.kHz(),
        clocks,
    );

    let i2c0_bus = blocking_mutex::Mutex::<blocking_mutex::raw::CriticalSectionRawMutex, _>::new(RefCell::new(i2c0));
    let i2c0_bus_static = make_static!(i2c0_bus);

    let mut i2c0_dev0 = blocking::i2c::I2cDevice::new(i2c0_bus_static);

    let sclk = io.pins.gpio36;
    let mosi = io.pins.gpio35;
    let miso = io.pins.gpio37;
    let dc = io.pins.gpio40.into_push_pull_output();
    let cs = io.pins.gpio42.into_push_pull_output();
    let rst = io.pins.gpio41.into_push_pull_output();
    let bl = io.pins.gpio45.into_push_pull_output();

    let spi2 = Spi::new(peripherals.SPI2, 26u32.MHz(), SpiMode::Mode0, clocks)
        .with_pins(Some(sclk), Some(mosi), Some(miso), Some(cs));

    let spi_iface = SPIInterfaceNoCS::new(spi2, dc);

    let mut delay = Delay;

    let display_size = Size::new(240, 135);

    let mut display = ST7789::new(
        spi_iface,
        Some(rst),
        Some(bl),
        240,
        135,
    );


    let background_color_default = Rgb565::BLACK;
    let display_width = display_size.width;
    let display_height = display_size.height;

    // initialize`
    display.init(&mut Delay).unwrap();

    // set default orientation
    display.set_orientation(Orientation::Landscape).unwrap();

    display.clear(Rgb565::BLACK).unwrap();

    let background_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb565::BLACK)
        .build();

    let red_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb565::RED)
        .build();

    let green_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb565::GREEN)
        .build();


    let center_text_style = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Middle)
        .build();

    let left_text_style = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Top)
        .build();

    let mut voltage_segment_style = SevenSegmentStyleBuilder::new()
        .digit_size(Size::new(30, 70)) // digits are 10x20 pixels
        .digit_spacing(5)              // 5px spacing between digits
        .segment_width(10)              // 5px wide segments
        .segment_color(Rgb565::WHITE)  // active segments are green
        .build();
    voltage_segment_style.inactive_segment_color = Some(background_color_default);

    let mut large_character_style = MonoTextStyle::new(
        &PROFONT_24_POINT,
        Rgb565::WHITE);
    large_character_style.background_color = Some(background_color_default);

    let mut power_display_buf: String<64> = String::from("");
    let mut unit_display_buf: String<2> = String::from("");
    let unit_display_width = (large_character_style.font.character_size.width * 2) as i32;

    spawner.must_spawn(handle_button_d1(io.pins.gpio1));
    spawner.must_spawn(handle_button_d2(io.pins.gpio2));
    spawner.must_spawn(handle_power(i2c0_dev0));

    let rect_size = Size::new(20, 20);

    let rect_top_red = Rectangle::new(create_point(10, 10), rect_size).into_styled(red_style);
    let rect_top_green = Rectangle::new(create_point(10, 10), rect_size).into_styled(green_style);

    let rect_bottom_red = Rectangle::new(create_point(10, display_height as i32 - 10 - rect_size.height as i32), rect_size).into_styled(red_style);
    let rect_bottom_green = Rectangle::new(create_point(10, display_height as i32 - 10 - rect_size.height as i32), rect_size).into_styled(green_style);

    let mut power_display: PowerDisplay = PowerDisplay::Shunt;

    loop {
        let input_data = INPUT_CHANNEL.receive().await;
        if input_data.button != 0 {
            match input_data.button {
                1 => {
                    if input_data.is_low {
                        // rect_top_red.draw(&mut display);
                    }
                    if input_data.is_high {
                        // rect_top_green.draw(&mut display);
                        power_display = power_display.previous().unwrap_or(PowerDisplay::Shunt);
                    }
                }
                2 => {
                    if input_data.is_low {
                        // rect_bottom_red.draw(&mut display);
                    }
                    if input_data.is_high {
                        // rect_bottom_green.draw(&mut display);
                        power_display = power_display.next().unwrap_or(PowerDisplay::Power);
                    }
                }
                _ => {}
            }
        } else {
            power_display_buf.clear();
            unit_display_buf.clear();

            match power_display {
                PowerDisplay::Shunt => {
                    write!(power_display_buf, "{:.3}", input_data.power.Shunt).unwrap();
                    write!(unit_display_buf, "mV").unwrap();
                }
                PowerDisplay::Voltage => {
                    write!(power_display_buf, "{:.3}", input_data.power.Voltage).unwrap();
                    write!(unit_display_buf, "V ").unwrap();
                }
                PowerDisplay::Current => {
                    write!(power_display_buf, "{:>5}", input_data.power.Current).unwrap();
                    write!(unit_display_buf, "mA").unwrap();
                }
                PowerDisplay::Power => {
                    write!(power_display_buf, "{:>5}", input_data.power.Power).unwrap();
                    write!(unit_display_buf, "mW").unwrap();
                }
            }
            GraphicUtils::display_text_with_background(&mut display, create_point(20, display_height as i32 / 2), voltage_segment_style, center_text_style, power_display_buf.as_str(), background_style, display_width);
            GraphicUtils::display_text_with_background(&mut display, create_point(display_width as i32 - unit_display_width, display_height as i32 / 2), large_character_style, center_text_style, unit_display_buf.as_str(), background_style, display_width);
        }
    }
}
