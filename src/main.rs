#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use alloc::format;
use core::cell::RefCell;
use core::fmt::Write;

use display_interface_spi::SPIInterfaceNoCS;
use eg_seven_segment::SevenSegmentStyleBuilder;
use embassy_embedded_hal::shared_bus::blocking;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Duration, Ticker, Timer};
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
use enum_iterator::cardinality;
use enum_iterator::Sequence;
use esp32_utils_crate::dummy_pin::DummyPin;
use esp32_utils_crate::graphics::GraphicUtils;
use esp_backtrace;
use esp_hal::{clock::ClockControl, embassy, IO, peripherals::Peripherals, prelude::*, psram};
use esp_hal::clock::Clocks;
use esp_hal::gpio::{GpioPin, Unknown};
use esp_hal::i2c::I2C;
use esp_hal::ledc::{channel, LEDC, LowSpeed, LSGlobalClkSource, timer};
use esp_hal::peripherals::I2C0;
use esp_hal::spi::master::Spi;
use esp_hal::spi::SpiMode;
use esp_hal::timer::TimerGroup;
use esp_println::println;
use heapless::String;
use ina219_rs::ina219::{Calibration, INA219, INA219_ADDR, PowerMonitor};
use profont::{PROFONT_12_POINT, PROFONT_18_POINT, PROFONT_24_POINT};
use st7789::{Orientation, ST7789};
use static_cell::{make_static, StaticCell};

use crate::max1704x::Max17048;

mod max1704x;

const ROWSTART: i32 = 40;
const COLSTART: i32 = 54;

#[derive(Debug, Clone)]
struct InputData {
    button: i8,
    power: PowerMonitor,
    msg: Option<heapless::String<128>>,
}

impl InputData {
    fn new() -> Self {
        InputData {
            button: -1,
            power: PowerMonitor {
                Shunt: 0.0,
                Voltage: 0.0,
                Current: 0.0,
                Power: 0.0,
            },
            msg: None,
        }
    }
}

static INPUT_CHANNEL: embassy_sync::channel::Channel<CriticalSectionRawMutex, InputData, 1> = embassy_sync::channel::Channel::new();

static CALIBRATION_SIGNAL: embassy_sync::signal::Signal<CriticalSectionRawMutex, Calibration> = embassy_sync::signal::Signal::new();

#[global_allocator]
static ALLOCATOR: esp_alloc::EspHeap = esp_alloc::EspHeap::empty();

#[derive(Debug, PartialEq, Sequence)]
enum PowerDisplay {
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
pub async fn handle_button_d0(pin: GpioPin<Unknown, 0>) {
    let mut button = pin.into_pull_up_input();
    loop {
        button.wait_for_low().await.unwrap();
        let mut input_data = InputData::new();
        input_data.button = 0;
        INPUT_CHANNEL.send(input_data).await;
        Timer::after(Duration::from_millis(500)).await
    }
}

#[embassy_executor::task]
pub async fn handle_button_d1(pin: GpioPin<Unknown, 1>) {
    let mut button = pin.into_pull_down_input();
    loop {
        button.wait_for_high().await.unwrap();
        let mut input_data = InputData::new();
        input_data.button = 1;
        INPUT_CHANNEL.send(input_data).await;
        Timer::after(Duration::from_millis(500)).await
    }
}

#[embassy_executor::task]
pub async fn handle_button_d2(pin: GpioPin<Unknown, 2>) {
    let mut button = pin.into_pull_down_input();
    loop {
        button.wait_for_high().await.unwrap();
        let mut input_data = InputData::new();
        input_data.button = 2;
        INPUT_CHANNEL.send(input_data).await;
        Timer::after(Duration::from_millis(500)).await
    }
}

#[embassy_executor::task]
pub async fn handle_power(i2c: blocking::i2c::I2cDevice<'static, CriticalSectionRawMutex, I2C<'static, I2C0>>) {
    let mut ina219 = INA219::new(i2c);
    match ina219.init(Calibration::Calibration_32V_2A) {
        Err(e) => {
            println!("{:?}", e);
            return;
        }
        _ => {}
    }

    let mut ticker = Ticker::every(Duration::from_millis(1000));
    loop {
        if CALIBRATION_SIGNAL.signaled() {
            let cal = CALIBRATION_SIGNAL.wait().await;
            ina219.init(cal.clone()).unwrap();
            Timer::after(Duration::from_secs(2)).await
        }
        if let Ok(power_monitor) = ina219.sense() {
            let mut input_data = InputData::new();
            input_data.power = power_monitor;
            INPUT_CHANNEL.send(input_data).await;
        }
        ticker.next().await;
    }
}

fn get_calibration(index: usize) -> Calibration {
    match index {
        0 => Calibration::Calibration_32V_2A,
        1 => Calibration::Calibration_32V_1A,
        2 => Calibration::Calibration_16V_400mA,
        _ => Calibration::Calibration_32V_2A
    }
}

fn get_calibration_text(cal: Calibration) -> heapless::String<128> {
    match cal {
        Calibration::Calibration_32V_2A => "32V - 2A".parse().unwrap(),
        Calibration::Calibration_32V_1A => "32V - 1A".parse().unwrap(),
        Calibration::Calibration_16V_400mA => "16V - 400mA".parse().unwrap(),
    }
}

// fn get_calibration_index(cal: Calibration) -> i8 {
//     match cal {
//         Calibration::Calibration_32V_2A => 0,
//         Calibration::Calibration_32V_1A => 1,
//         Calibration::Calibration_16V_400mA => 2,
//     }
// }

// fn get_calibration_indicator_pos(index: usize, display_size: Size, indicator_size: Size) -> Point {
//     match index {
//         0 => create_point(0, 0),
//         1 => create_point(0, (display_size.height / 2 - indicator_size.height / 2 - 10) as i32),
//         2 => create_point(0, display_size.height as i32 - 10 - indicator_size.height as i32),
//         _ => create_point(0, 0)
//     }
// }

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

    let i2c0 = I2C::new(
        peripherals.I2C0,
        io.pins.gpio3,
        io.pins.gpio4,
        100u32.kHz(),
        clocks,
    );

    let i2c0_bus = blocking_mutex::Mutex::<blocking_mutex::raw::CriticalSectionRawMutex, _>::new(RefCell::new(i2c0));
    let i2c0_bus_static = make_static!(i2c0_bus);

    let mut i2c0_dev0 = blocking::i2c::I2cDevice::new(i2c0_bus_static);
    let mut i2c0_dev1 = blocking::i2c::I2cDevice::new(i2c0_bus_static);

    let has_ina219 = i2c0_dev0.read(INA219_ADDR, &mut [0]).is_ok();
    println!("has_ina219 = {}", has_ina219);

    // TODO lipo battery monitor at 0x36
    // https://github.com/adafruit/Adafruit_CircuitPython_MAX1704x/blob/main/adafruit_max1704x.py
    let has_lipo_monitor = i2c0_dev1.read(0x36, &mut [0]).is_ok();
    println!("has_lipo_monitor = {}", has_lipo_monitor);

    let mut lipo = Max17048::new(i2c0_dev1);

    let sclk = io.pins.gpio36;
    let mosi = io.pins.gpio35;
    let miso = io.pins.gpio37;
    let dc = io.pins.gpio40.into_push_pull_output();
    let cs = io.pins.gpio42.into_push_pull_output();
    let rst = io.pins.gpio41.into_push_pull_output();
    let bl = io.pins.gpio45.into_push_pull_output();

    let mut ledc = LEDC::new(peripherals.LEDC, &clocks);

    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer0 = ledc.get_timer::<LowSpeed>(timer::Number::Timer0);

    lstimer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty5Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: 24u32.kHz(),
        })
        .unwrap();

    let mut channel0 = ledc.get_channel(channel::Number::Channel0, bl);
    channel0
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 50,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    let spi2 = Spi::new(peripherals.SPI2, 40u32.MHz(), SpiMode::Mode0, clocks)
        .with_pins(Some(sclk), Some(mosi), Some(miso), Some(cs));

    let spi_iface = SPIInterfaceNoCS::new(spi2, dc);

    let display_size = Size::new(240, 135);

    let mut display = ST7789::new(
        spi_iface,
        Some(rst),
        Some(DummyPin),
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

    // let red_style = PrimitiveStyleBuilder::new()
    //     .fill_color(Rgb565::RED)
    //     .build();
    //
    // let green_style = PrimitiveStyleBuilder::new()
    //     .fill_color(Rgb565::GREEN)
    //     .build();

    let center_text_style = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Middle)
        .build();

    let left_text_style = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Top)
        .build();

    let mut voltage_segment_style = SevenSegmentStyleBuilder::new()
        .digit_size(Size::new(30, 80)) // digits are 10x20 pixels
        .digit_spacing(5)              // 5px spacing between digits
        .segment_width(10)              // 5px wide segments
        .segment_color(Rgb565::WHITE)  // active segments are green
        .build();
    voltage_segment_style.inactive_segment_color = Some(background_color_default);

    let mut large_character_style = MonoTextStyle::new(
        &PROFONT_24_POINT,
        Rgb565::WHITE);
    large_character_style.background_color = Some(background_color_default);

    let mut medium_character_style = MonoTextStyle::new(
        &PROFONT_18_POINT,
        Rgb565::WHITE);
    medium_character_style.background_color = Some(background_color_default);

    let mut small_character_style = MonoTextStyle::new(
        &PROFONT_12_POINT,
        Rgb565::WHITE);
    small_character_style.background_color = Some(background_color_default);

    let mut power_display_buf: String<64> = String::new();
    let mut unit_display_buf: String<2> = String::new();
    let unit_display_width = (large_character_style.font.character_size.width * 2) as i32;

    spawner.must_spawn(handle_button_d0(io.pins.gpio0));
    spawner.must_spawn(handle_button_d1(io.pins.gpio1));
    spawner.must_spawn(handle_button_d2(io.pins.gpio2));
    if has_ina219 {
        spawner.must_spawn(handle_power(i2c0_dev0));
    } else {
        let _ = GraphicUtils::display_text_with_background(&mut display,
                                                           create_point(0, (display_height / 2) as i32),
                                                           large_character_style,
                                                           center_text_style,
                                                           "No ina219 found",
                                                           background_style,
                                                           display_width).unwrap();
    }

    if has_lipo_monitor {
        let mut battery_voltage: String<64> = String::new();
        let lipo_voltage = lipo.vcell().unwrap();
        write!(battery_voltage, "Battery {:1.1} V", lipo_voltage).unwrap();
        let _ = GraphicUtils::display_text_with_background(&mut display,
                                                           create_point(0, (display_height / 2) as i32),
                                                           large_character_style,
                                                           center_text_style,
                                                           battery_voltage.as_str(),
                                                           background_style,
                                                           display_width).unwrap();
        Timer::after(Duration::from_secs(5)).await
    }

    let mut cal_index = 0;

    // let rect_size = Size::new(20, 20);

    // let rect_top_red = Rectangle::new(create_point(0, 10), rect_size).into_styled(red_style);
    // let rect_top_green = Rectangle::new(create_point(0, 10), rect_size).into_styled(green_style);
    //
    // let rect_middle_red = Rectangle::new(create_point(0, (display_height / 2 - rect_size.height / 2) as i32), rect_size).into_styled(red_style);
    // let rect_middle_green = Rectangle::new(create_point(0, (display_height / 2 - rect_size.height / 2) as i32), rect_size).into_styled(green_style);
    //
    // let rect_bottom_red = Rectangle::new(create_point(0, display_height as i32 - 10 - rect_size.height as i32), rect_size).into_styled(red_style);
    // let rect_bottom_green = Rectangle::new(create_point(0, display_height as i32 - 10 - rect_size.height as i32), rect_size).into_styled(green_style);

    let mut power_display: PowerDisplay = PowerDisplay::Voltage;

    let mut last_power_display_buf: String<64> = String::new();

    loop {
        let mut input_data = INPUT_CHANNEL.receive().await;
        if input_data.button != -1 {
            match input_data.button {
                0 => {
                    // rect_top_red.draw(&mut display);
                    cal_index = (cal_index + 1) % cardinality::<Calibration>();
                    CALIBRATION_SIGNAL.signal(get_calibration(cal_index));
                    input_data.msg = Some(get_calibration_text(get_calibration(cal_index)).clone());
                    last_power_display_buf.clear();
                }
                1 => {
                    // rect_middle_green.draw(&mut display);
                    power_display = power_display.previous().unwrap_or(PowerDisplay::Voltage);
                }
                2 => {
                    power_display = power_display.next().unwrap_or(PowerDisplay::Power);
                }
                _ => {}
            }
        }
        power_display_buf.clear();
        unit_display_buf.clear();

        match power_display {
            // PowerDisplay::Shunt => {
            //     if input_data.power.Current != 0.0 {
            //         write!(power_display_buf, "{:.3}", input_data.power.Shunt).unwrap();
            //     } else {
            //         write!(power_display_buf, "{:.3}", 0.0).unwrap();
            //     }
            //     write!(unit_display_buf, "mV").unwrap();
            // }
            PowerDisplay::Voltage => {
                if input_data.power.Current != 0.0 {
                    write!(power_display_buf, "{:>2.3}", input_data.power.Voltage).unwrap();
                } else {
                    write!(power_display_buf, "{:>2.3}", 0.0).unwrap();
                }
                write!(unit_display_buf, "V ").unwrap();
            }
            PowerDisplay::Current => {
                if input_data.power.Current != 0.0 {
                    write!(power_display_buf, "{:>5}", input_data.power.Current).unwrap();
                } else {
                    write!(power_display_buf, "{:>5}", 0.0).unwrap();
                }
                write!(unit_display_buf, "mA").unwrap();
            }
            PowerDisplay::Power => {
                if input_data.power.Current != 0.0 {
                    write!(power_display_buf, "{:>5}", input_data.power.Power).unwrap();
                } else {
                    write!(power_display_buf, "{:>5}", 0.0).unwrap();
                }
                write!(unit_display_buf, "mW").unwrap();
            }
        }
        // Rectangle::new(get_calibration_indicator_pos(cal_index, display_size, rect_size), rect_size).into_styled(green_style).draw(&mut display);
        if input_data.msg.is_some() {
            let _ = Rectangle::new(create_point(0, 0), display_size).into_styled(background_style).draw(&mut display);
            let _ = GraphicUtils::display_text(&mut display, create_point(10, (display_height / 2) as i32), large_character_style, center_text_style, input_data.msg.unwrap().as_str());
        } else {
            if power_display_buf != last_power_display_buf {
                let _ = GraphicUtils::display_text_with_background(&mut display, create_point(0, (display_height / 2) as i32), voltage_segment_style, center_text_style, power_display_buf.as_str(), background_style, display_width);
                let _ = GraphicUtils::display_text_with_background(&mut display, create_point(display_width as i32 - unit_display_width, (display_height / 2) as i32), large_character_style, center_text_style, unit_display_buf.as_str(), background_style, display_width);
            }
            last_power_display_buf = String::from(power_display_buf.clone());
        }
    }
}
