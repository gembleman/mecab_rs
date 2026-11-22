use std::io::{self, Write};
use std::path::Path;

use super::{FeatureIndex, Settings, TOO_MANY_IDS, weight_index};
use crate::build::Charset;
use crate::build::text::{LINE_BUFFER_BYTES, Lines, atof, cstr, tokenize2};
use crate::learn::{LineEnding, LineEnds};
use crate::tool_error::{Position, caused_by, learn_error};
use mecab_rs::__private::files::write_file;
use mecab_rs::Error;

fn read_model_header(lines: &mut Lines, path: &Path) -> Result<(String, Settings), Error> {
    let mut model_charset = String::new();
    let mut settings = Vec::new();
    while let Some((number, line)) = lines.next_line()? {
        if line[0] == 0 {
            break;
        }
        let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
        let found = tokenize2(line, b":", 2);
        if found.len() != 2 {
            return Err(Position::new(path, number).learn(format!("format error: {shown}")));
        }
        let key = String::from_utf8_lossy(cstr(line, found[0])).into_owned();
        let value =
            String::from_utf8_lossy(cstr(line, found[1]).get(1..).unwrap_or_default()).into_owned();
        if key == "charset" {
            model_charset = value;
        } else {
            settings.push((key, value));
        }
    }
    if model_charset.is_empty() {
        return Err(learn_error(path, "charset is empty"));
    }
    Ok((model_charset, settings))
}

fn require_same_charset(
    path: &Path,
    model_charset: &str,
    dictionary_charset: &str,
) -> Result<(), Error> {
    let old_charset = Charset::parse(model_charset).map_err(|reason| learn_error(path, reason))?;
    let new_charset =
        Charset::parse(dictionary_charset).map_err(|reason| learn_error(path, reason))?;
    if old_charset != new_charset {
        return Err(learn_error(
            path,
            format!("cannot create model from={model_charset} to={dictionary_charset}"),
        ));
    }
    Ok(())
}

impl FeatureIndex {
    pub(in crate::learn) fn reopen(
        &mut self,
        path: &Path,
        dictionary_charset: &str,
    ) -> Result<(Vec<f64>, Settings), Error> {
        self.dic.clear();
        self.cache.clear();
        self.maxid = 0;
        let mut lines =
            Lines::open(path, Some(LINE_BUFFER_BYTES)).map_err(|error| match error {
                Error::Io { source, .. } => caused_by(
                    learn_error(
                        path,
                        format!("cannot open the old model {}", path.display()),
                    ),
                    source,
                ),
                other => other,
            })?;
        let (model_charset, settings) = read_model_header(&mut lines, path)?;
        require_same_charset(path, &model_charset, dictionary_charset)?;
        let alpha = self.read_old_weights(&mut lines, path)?;
        Ok((alpha, settings))
    }

    fn read_old_weights(&mut self, lines: &mut Lines, path: &Path) -> Result<Vec<f64>, Error> {
        let mut alpha = Vec::new();
        while let Some((number, line)) = lines.next_line()? {
            let shown = String::from_utf8_lossy(cstr(line, 0)).into_owned();
            let found = tokenize2(line, b"\t", 2);
            if found.len() != 2 {
                return Err(Position::new(path, number).learn(format!("format error: {shown}")));
            }
            let id = i32::try_from(self.maxid).map_err(|_| learn_error(path, TOO_MANY_IDS))?;
            self.dic.entry(cstr(line, found[1]).to_vec()).or_insert(id);
            self.maxid += 1;
            alpha.push(atof(cstr(line, found[0])));
        }
        Ok(alpha)
    }

    pub(in crate::learn) fn save(
        &self,
        path: &Path,
        header: &str,
        alpha: &[f64],
        ending: LineEnding,
    ) -> Result<(), Error> {
        let mut entries: Vec<(&Vec<u8>, i32)> =
            self.dic.iter().map(|(key, &id)| (key, id)).collect();
        entries.sort_unstable();
        if let Some((key, _)) = entries
            .iter()
            .find(|&&(_, id)| !alpha[weight_index(id)].is_finite())
        {
            return Err(learn_error(
                path,
                format!(
                    "the weight of `{}` is not finite",
                    String::from_utf8_lossy(key)
                ),
            ));
        }
        write_file(path, |file| {
            let mut file = LineEnds::new(file, ending);
            file.write_all(header.as_bytes())?;
            file.write_all(b"\n")?;
            let mut line = Vec::new();
            for (key, id) in entries {
                line.clear();
                write!(line, "{:.16}\t", alpha[weight_index(id)])?;
                line.extend_from_slice(key);
                line.push(b'\n');
                file.write_all(&line)?;
            }
            io::Result::Ok(())
        })
    }
}
