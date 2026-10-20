//! 物理输入（按键 / 侧边开关）与它们可绑定的动作。
//!
//! 固件把每个输入要执行的动作存在设备 KV 配置里（`CONFIG_SET/GET/DEL`，键见
//! [`ControlInput::config_key`]）。输入触发时固件先上报 `BTN_CLICK` /
//! `TOGGLE_SWITCH` 事件（带当前绑定的动作），再**在设备侧**执行该动作——上位机
//! 不需要、也不应该再替它执行一遍。没配置过的输入等价于 [`ControlAction::None`]。

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SdkError;

/// 按键手势（`BTN_CLICK` 事件的 `key` 字段）。
///
/// `LongPress` 在按住达到长按阈值时上报，松手后再补一个配对的 `LongRelease`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonGesture {
    Single,
    Double,
    LongPress,
    LongRelease,
}

impl ButtonGesture {
    pub const ALL: [Self; 4] = [
        Self::Single,
        Self::Double,
        Self::LongPress,
        Self::LongRelease,
    ];

    /// 事件 `key` 字段里的名字。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Double => "double",
            Self::LongPress => "long_press",
            Self::LongRelease => "long_release",
        }
    }

    /// 存放该手势绑定动作的 KV 配置键。
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Single => "btn_single",
            Self::Double => "btn_double",
            Self::LongPress => "btn_long_press",
            Self::LongRelease => "btn_long_release",
        }
    }
}

impl fmt::Display for ButtonGesture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl FromStr for ButtonGesture {
    type Err = SdkError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|gesture| gesture.wire_name() == text)
            .ok_or_else(|| SdkError::InvalidFrame(format!("unknown button gesture `{text}`")))
    }
}

/// 侧边开关位置（`TOGGLE_SWITCH` 事件的 `state` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchPosition {
    On,
    Off,
}

impl SwitchPosition {
    pub const ALL: [Self; 2] = [Self::On, Self::Off];

    /// 事件 `state` 字段里的名字。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// 存放该位置绑定动作的 KV 配置键。
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::On => "switch_on",
            Self::Off => "switch_off",
        }
    }
}

impl fmt::Display for SwitchPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl FromStr for SwitchPosition {
    type Err = SdkError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|position| position.wire_name() == text)
            .ok_or_else(|| SdkError::InvalidFrame(format!("unknown switch position `{text}`")))
    }
}

/// 可以绑定动作的一个输入：按键的某个手势，或开关的某个位置。
///
/// [`ButtonGesture`] / [`SwitchPosition`] 都能 `Into` 成它，所以绑定接口直接传
/// `ButtonGesture::Single` 这类值即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlInput {
    Button(ButtonGesture),
    Switch(SwitchPosition),
}

impl ControlInput {
    /// 固件支持的全部六个输入，顺序与 [`ControlBindings`] 的字段一致。
    pub const ALL: [Self; 6] = [
        Self::Button(ButtonGesture::Single),
        Self::Button(ButtonGesture::Double),
        Self::Button(ButtonGesture::LongPress),
        Self::Button(ButtonGesture::LongRelease),
        Self::Switch(SwitchPosition::On),
        Self::Switch(SwitchPosition::Off),
    ];

    /// 存放绑定动作的 KV 配置键（`btn_*` / `switch_*`）。
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Button(gesture) => gesture.config_key(),
            Self::Switch(position) => position.config_key(),
        }
    }

    /// 由配置键反查输入；非绑定键返回 `None`。
    pub fn from_config_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|input| input.config_key() == key)
    }
}

impl From<ButtonGesture> for ControlInput {
    fn from(gesture: ButtonGesture) -> Self {
        Self::Button(gesture)
    }
}

impl From<SwitchPosition> for ControlInput {
    fn from(position: SwitchPosition) -> Self {
        Self::Switch(position)
    }
}

impl fmt::Display for ControlInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.config_key())
    }
}

/// 输入触发时设备执行的动作（KV 配置值，也是事件里的 `action` 字段）。
///
/// 固件只认前五个；[`Other`](Self::Other) 用来无损携带未来固件新增的值——
/// 当前固件收到不认识的值会上报原字符串但不执行任何动作。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ControlAction {
    /// 不执行动作（也是未配置时的默认值）。
    #[default]
    None,
    RecordStart,
    RecordStop,
    /// 录音中则停止，否则开始。
    RecordToggle,
    /// 录音中打一个标记点（未在录音时固件忽略）。
    RecordMark,
    Other(String),
}

impl ControlAction {
    /// 当前固件认识的全部动作。
    pub const KNOWN: [Self; 5] = [
        Self::None,
        Self::RecordStart,
        Self::RecordStop,
        Self::RecordToggle,
        Self::RecordMark,
    ];

    /// 配置值 / 事件字段里的名字。
    pub fn wire_name(&self) -> &str {
        match self {
            Self::None => "none",
            Self::RecordStart => "record_start",
            Self::RecordStop => "record_stop",
            Self::RecordToggle => "record_toggle",
            Self::RecordMark => "record_mark",
            Self::Other(name) => name,
        }
    }

    /// 从 wire 名解析；不认识的值进 [`Other`](Self::Other)，不会失败。
    pub fn parse(text: &str) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|action| action.wire_name() == text)
            .unwrap_or_else(|| Self::Other(text.to_owned()))
    }
}

impl From<String> for ControlAction {
    fn from(text: String) -> Self {
        Self::parse(&text)
    }
}

impl From<ControlAction> for String {
    fn from(action: ControlAction) -> Self {
        match action {
            ControlAction::Other(name) => name,
            known => known.wire_name().to_owned(),
        }
    }
}

impl FromStr for ControlAction {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(text))
    }
}

impl fmt::Display for ControlAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// `BTN_CLICK` 事件：按键完成了某个手势，`action` 是设备随之执行的绑定动作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonEvent {
    pub gesture: ButtonGesture,
    pub action: ControlAction,
}

impl ButtonEvent {
    /// 解析固件载荷 `{"key":"single","action":"none"}`；`key` 不认识返回 `None`，
    /// `action` 缺失按未配置（[`ControlAction::None`]）处理。
    pub(crate) fn from_payload(payload: &Value) -> Option<Self> {
        let gesture = payload.get("key")?.as_str()?.parse().ok()?;
        Some(Self {
            gesture,
            action: action_field(payload),
        })
    }
}

/// `TOGGLE_SWITCH` 事件：侧边开关被拨到了 `position`，`action` 同上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToggleSwitchEvent {
    pub position: SwitchPosition,
    pub action: ControlAction,
}

impl ToggleSwitchEvent {
    /// 解析固件载荷 `{"state":"on","action":"record_start"}`；`state` 不认识返回 `None`。
    pub(crate) fn from_payload(payload: &Value) -> Option<Self> {
        let position = payload.get("state")?.as_str()?.parse().ok()?;
        Some(Self {
            position,
            action: action_field(payload),
        })
    }
}

fn action_field(payload: &Value) -> ControlAction {
    payload
        .get("action")
        .and_then(Value::as_str)
        .map(ControlAction::parse)
        .unwrap_or_default()
}

/// 六个输入的完整绑定表，字段顺序与 [`ControlInput::ALL`] 一致。
///
/// `Default` 是全部 [`ControlAction::None`]（即出厂状态）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ControlBindings {
    pub button_single: ControlAction,
    pub button_double: ControlAction,
    pub button_long_press: ControlAction,
    pub button_long_release: ControlAction,
    pub switch_on: ControlAction,
    pub switch_off: ControlAction,
}

impl ControlBindings {
    /// 录音笔式预设（仅是建议）：开关拨到 on 开始录音、拨回 off 停止；
    /// 单击在录音中打标记，双击切换录音；长按不绑定（留给设备自身的指示用途）。
    pub fn recorder() -> Self {
        Self {
            button_single: ControlAction::RecordMark,
            button_double: ControlAction::RecordToggle,
            button_long_press: ControlAction::None,
            button_long_release: ControlAction::None,
            switch_on: ControlAction::RecordStart,
            switch_off: ControlAction::RecordStop,
        }
    }

    pub fn get(&self, input: impl Into<ControlInput>) -> &ControlAction {
        match input.into() {
            ControlInput::Button(ButtonGesture::Single) => &self.button_single,
            ControlInput::Button(ButtonGesture::Double) => &self.button_double,
            ControlInput::Button(ButtonGesture::LongPress) => &self.button_long_press,
            ControlInput::Button(ButtonGesture::LongRelease) => &self.button_long_release,
            ControlInput::Switch(SwitchPosition::On) => &self.switch_on,
            ControlInput::Switch(SwitchPosition::Off) => &self.switch_off,
        }
    }

    pub fn set(&mut self, input: impl Into<ControlInput>, action: ControlAction) {
        *self.slot(input.into()) = action;
    }

    /// 按 [`ControlInput::ALL`] 的顺序遍历全部绑定。
    pub fn entries(&self) -> impl Iterator<Item = (ControlInput, &ControlAction)> {
        ControlInput::ALL
            .into_iter()
            .map(|input| (input, self.get(input)))
    }

    fn slot(&mut self, input: ControlInput) -> &mut ControlAction {
        match input {
            ControlInput::Button(ButtonGesture::Single) => &mut self.button_single,
            ControlInput::Button(ButtonGesture::Double) => &mut self.button_double,
            ControlInput::Button(ButtonGesture::LongPress) => &mut self.button_long_press,
            ControlInput::Button(ButtonGesture::LongRelease) => &mut self.button_long_release,
            ControlInput::Switch(SwitchPosition::On) => &mut self.switch_on,
            ControlInput::Switch(SwitchPosition::Off) => &mut self.switch_off,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn wire_names_round_trip_through_parse_and_display() {
        for gesture in ButtonGesture::ALL {
            assert_eq!(
                gesture.to_string().parse::<ButtonGesture>().unwrap(),
                gesture
            );
        }
        for position in SwitchPosition::ALL {
            assert_eq!(
                position.to_string().parse::<SwitchPosition>().unwrap(),
                position
            );
        }
        for action in ControlAction::KNOWN {
            assert_eq!(ControlAction::parse(action.wire_name()), action);
        }
        assert!("triple".parse::<ButtonGesture>().is_err());
        assert!("middle".parse::<SwitchPosition>().is_err());
    }

    #[test]
    fn config_keys_match_firmware_main_c() {
        let keys: Vec<&str> = ControlInput::ALL
            .into_iter()
            .map(ControlInput::config_key)
            .collect();
        assert_eq!(
            keys,
            [
                "btn_single",
                "btn_double",
                "btn_long_press",
                "btn_long_release",
                "switch_on",
                "switch_off"
            ]
        );
        assert_eq!(
            ControlInput::from_config_key("switch_off"),
            Some(ControlInput::Switch(SwitchPosition::Off))
        );
        assert_eq!(ControlInput::from_config_key("wifi_api_base_url"), None);
    }

    #[test]
    fn unknown_action_is_kept_verbatim() {
        let action = ControlAction::parse("led_blink");
        assert_eq!(action, ControlAction::Other("led_blink".into()));
        assert_eq!(action.wire_name(), "led_blink");
        assert_eq!(String::from(action), "led_blink");
    }

    #[test]
    fn action_serializes_as_wire_string() {
        assert_eq!(
            serde_json::to_value(ControlAction::RecordToggle).unwrap(),
            json!("record_toggle")
        );
        assert_eq!(
            serde_json::from_value::<ControlAction>(json!("record_mark")).unwrap(),
            ControlAction::RecordMark
        );
        assert_eq!(
            serde_json::from_value::<ControlAction>(json!("future")).unwrap(),
            ControlAction::Other("future".into())
        );
    }

    #[test]
    fn parses_firmware_event_payloads() {
        assert_eq!(
            ButtonEvent::from_payload(&json!({"key":"long_release","action":"record_stop"})),
            Some(ButtonEvent {
                gesture: ButtonGesture::LongRelease,
                action: ControlAction::RecordStop,
            })
        );
        // action 缺失 → 未配置
        assert_eq!(
            ButtonEvent::from_payload(&json!({"key":"single"})),
            Some(ButtonEvent {
                gesture: ButtonGesture::Single,
                action: ControlAction::None,
            })
        );
        assert_eq!(
            ButtonEvent::from_payload(&json!({"key":"triple","action":"none"})),
            None
        );
        assert_eq!(
            ToggleSwitchEvent::from_payload(&json!({"state":"off","action":"record_stop"})),
            Some(ToggleSwitchEvent {
                position: SwitchPosition::Off,
                action: ControlAction::RecordStop,
            })
        );
        assert_eq!(
            ToggleSwitchEvent::from_payload(&json!({"state":"mid"})),
            None
        );
    }

    #[test]
    fn bindings_table_is_addressable_by_input() {
        let mut bindings = ControlBindings::default();
        assert!(
            bindings
                .entries()
                .all(|(_, action)| *action == ControlAction::None)
        );
        bindings.set(ButtonGesture::Double, ControlAction::RecordToggle);
        bindings.set(SwitchPosition::On, ControlAction::RecordStart);
        assert_eq!(
            bindings.get(ButtonGesture::Double),
            &ControlAction::RecordToggle
        );
        assert_eq!(
            bindings.get(SwitchPosition::On),
            &ControlAction::RecordStart
        );
        assert_eq!(bindings.get(SwitchPosition::Off), &ControlAction::None);

        let recorder = ControlBindings::recorder();
        assert_eq!(recorder.switch_on, ControlAction::RecordStart);
        assert_eq!(recorder.switch_off, ControlAction::RecordStop);
        assert_eq!(recorder.entries().count(), 6);
    }
}
