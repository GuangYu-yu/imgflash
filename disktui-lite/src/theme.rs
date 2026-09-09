use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub focus_border: Color,
    pub header: Color,
    pub error: Color,
    pub success: Color,

    pub progress_bar_filled: &'static str,
    pub progress_bar_empty: &'static str,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            focus_border: Color::Indexed(2),
            header: Color::Indexed(3),
            error: Color::Indexed(1),
            success: Color::Indexed(2),

            progress_bar_filled: "█",
            progress_bar_empty: "░",
        }
    }
}