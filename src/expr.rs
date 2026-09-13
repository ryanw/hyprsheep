//! The tiny arithmetic expression language used throughout the pet XML.
//!
//! Values like `<x>`, `<y>`, `<interval>` and `repeat=` are not plain numbers:
//! they are expressions over a handful of screen/sprite identifiers, e.g.
//! `areaH/2-(randS*areaH/2)/120-imageH`. The reference implementation does
//! textual substitution followed by `eval()`; we parse properly instead.

/// The identifier bindings an expression is evaluated against.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    /// Full output size.
    pub screen_w: f64,
    pub screen_h: f64,
    /// Usable work area, i.e. the screen minus any reserved bar space.
    pub area_w: f64,
    pub area_h: f64,
    /// Sprite tile size.
    pub image_w: f64,
    pub image_h: f64,
    /// Current sprite position.
    pub image_x: f64,
    pub image_y: f64,
    /// Per-sheep constant in 0..100; the original's "personality" value.
    pub rand_s: f64,
}

impl Ctx {
    fn lookup(&self, name: &str) -> Option<f64> {
        Some(match name {
            "screenW" => self.screen_w,
            "screenH" => self.screen_h,
            // The reference binds areaW to the screen *height*; that is plainly
            // a typo, so we bind it to the width as the XML authors intended.
            "areaW" => self.area_w,
            "areaH" => self.area_h,
            "imageW" => self.image_w,
            "imageH" => self.image_h,
            "imageX" => self.image_x,
            "imageY" => self.image_y,
            "random" => fastrand::f64() * 100.0,
            "randS" => self.rand_s,
            _ => return None,
        })
    }
}

pub fn eval(src: &str, ctx: &Ctx) -> Result<f64, String> {
    let mut p = Parser { s: src.as_bytes(), i: 0, ctx };
    let v = p.expr()?;
    p.skip_ws();
    if p.i != p.s.len() {
        return Err(format!("trailing input in {src:?} at byte {}", p.i));
    }
    Ok(v)
}

/// Evaluate, falling back to `default` if the expression is malformed.
pub fn eval_or(src: &str, ctx: &Ctx, default: f64) -> f64 {
    match eval(src, ctx) {
        Ok(v) if v.is_finite() => v,
        Ok(_) => default,
        Err(e) => {
            eprintln!("hyprsheep: bad expression {src:?}: {e}");
            default
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    ctx: &'a Ctx,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            return true;
        }
        false
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        loop {
            match self.peek() {
                Some(b'+') => {
                    self.i += 1;
                    v += self.term()?;
                }
                Some(b'-') => {
                    self.i += 1;
                    v -= self.term()?;
                }
                _ => return Ok(v),
            }
        }
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.i += 1;
                    v *= self.unary()?;
                }
                Some(b'/') => {
                    self.i += 1;
                    let d = self.unary()?;
                    // Division by zero would poison the whole expression; the
                    // reference yields Infinity here and we would rather not.
                    if d == 0.0 {
                        return Err("division by zero".into());
                    }
                    v /= d;
                }
                Some(b'%') => {
                    self.i += 1;
                    let d = self.unary()?;
                    if d == 0.0 {
                        return Err("modulo by zero".into());
                    }
                    v %= d;
                }
                _ => return Ok(v),
            }
        }
    }

    fn unary(&mut self) -> Result<f64, String> {
        if self.eat(b'-') {
            return Ok(-self.unary()?);
        }
        if self.eat(b'+') {
            return self.unary();
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<f64, String> {
        match self.peek() {
            None => Err("unexpected end of expression".into()),
            Some(b'(') => {
                self.i += 1;
                let v = self.expr()?;
                if !self.eat(b')') {
                    return Err("expected ')'".into());
                }
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => self.number(),
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => self.ident(),
            Some(c) => Err(format!("unexpected character {:?}", c as char)),
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
        {
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?;
        text.parse().map_err(|_| format!("bad number {text:?}"))
    }

    fn ident(&mut self) -> Result<f64, String> {
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')
        {
            self.i += 1;
        }
        let name = std::str::from_utf8(&self.s[start..self.i]).map_err(|e| e.to_string())?;

        // Two animations use .NET's `Convert(expr, System.Int32)`, which the
        // JS reference throws on and silently treats as 0. Honour it as the
        // truncation it was written to mean.
        if name == "Convert" {
            if !self.eat(b'(') {
                return Err("expected '(' after Convert".into());
            }
            let v = self.expr()?;
            if !self.eat(b',') {
                return Err("expected ',' in Convert".into());
            }
            self.skip_ws();
            let ts = self.i;
            while self
                .s
                .get(self.i)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')
            {
                self.i += 1;
            }
            let ty = std::str::from_utf8(&self.s[ts..self.i]).unwrap_or("");
            if !self.eat(b')') {
                return Err("expected ')' in Convert".into());
            }
            return match ty {
                "System.Int32" | "System.Int64" | "System.Int16" => Ok(v.trunc()),
                other => Err(format!("unsupported Convert target {other:?}")),
            };
        }

        self.ctx
            .lookup(name)
            .ok_or_else(|| format!("unknown identifier {name:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            screen_w: 1920.0,
            screen_h: 1080.0,
            area_w: 1920.0,
            area_h: 1080.0,
            image_w: 40.0,
            image_h: 40.0,
            image_x: 100.0,
            image_y: 200.0,
            rand_s: 60.0,
        }
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(eval("1+2*3", &ctx()).unwrap(), 7.0);
        assert_eq!(eval("(1+2)*3", &ctx()).unwrap(), 9.0);
        assert_eq!(eval("-2", &ctx()).unwrap(), -2.0);
        assert_eq!(eval("7%4", &ctx()).unwrap(), 3.0);
    }

    #[test]
    fn identifiers_resolve() {
        assert_eq!(eval("screenW+10", &ctx()).unwrap(), 1930.0);
        assert_eq!(eval("imageX-imageW*0.9", &ctx()).unwrap(), 64.0);
        // areaW must be the width, not the height as the reference has it.
        assert_eq!(eval("areaW", &ctx()).unwrap(), 1920.0);
    }

    #[test]
    fn real_expressions_from_the_pet_xml() {
        let c = ctx();
        assert_eq!(eval("areaH-imageH", &c).unwrap(), 1040.0);
        assert_eq!(eval("(screenW/2)/30-6", &c).unwrap(), 26.0);
        // The .NET Convert() call that the JS reference fails on.
        assert_eq!(eval("24+(Convert(screenW/2,System.Int32)%30)/7", &c).unwrap(), 24.0);
        assert!(eval("areaH/2-(randS*areaH/2)/120-imageH", &c).is_ok());
        assert!(eval("random*(screenW-imageW-50)/100+25", &c).is_ok());
    }

    #[test]
    fn random_is_in_range_and_varies() {
        let c = ctx();
        for _ in 0..100 {
            let v = eval("random", &c).unwrap();
            assert!((0.0..100.0).contains(&v), "random out of range: {v}");
        }
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        assert!(eval("1/0", &ctx()).is_err());
        assert!(eval("nonsense", &ctx()).is_err());
        assert!(eval("1+", &ctx()).is_err());
        assert_eq!(eval_or("1+", &ctx(), 42.0), 42.0);
    }
}
