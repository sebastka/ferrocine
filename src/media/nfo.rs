//! Kodi-style `.nfo` files (`<movie>`, `<tvshow>`, `<episodedetails>`), read-only.

use quick_xml::Reader;
use quick_xml::events::Event;

use super::Person;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Nfo {
    pub title: Option<String>,
    pub original_title: Option<String>,
    pub sort_title: Option<String>,
    pub year: Option<i32>,
    pub premiered: Option<String>,
    pub plot: Option<String>,
    pub tagline: Option<String>,
    pub rating: Option<f64>,
    pub mpaa: Option<String>,
    pub genres: Vec<String>,
    pub studios: Vec<String>,
    pub people: Vec<Person>,
    pub tmdb: Option<String>,
    pub imdb: Option<String>,
    pub tvdb: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub runtime_minutes: Option<i64>,
}

fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Parse an NFO document. Files holding only a URL (some scrapers write
/// those) yield an empty result.
pub fn parse(text: &str) -> Nfo {
    let mut nfo = Nfo::default();
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    // Element names from the root, and the attributes that matter.
    let mut stack: Vec<String> = Vec::new();
    // Text of the innermost element; entity references arrive as separate events.
    let mut buf = String::new();
    let mut uniqueid_type: Option<String> = None;
    let mut actor: Option<Person> = None;
    let mut rating_default = false;
    let mut rating_value: Option<f64> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                buf.clear();
                let name = e.local_name().as_ref().to_lowercase();
                let attr = |key: &str| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == key)
                        .map(|a| a.value.to_lowercase())
                };
                match name.as_str() {
                    "uniqueid" => uniqueid_type = attr("type"),
                    "actor" if stack.len() == 1 => {
                        actor = Some(Person {
                            role: "actor".into(),
                            ..Default::default()
                        });
                    }
                    "rating" if stack.last().is_some_and(|p| p == "ratings") => {
                        rating_default = attr("default").as_deref() == Some("true");
                    }
                    _ => {}
                }
                stack.push(name);
            }
            Ok(Event::Text(t)) => buf.push_str(&t.xml10_content()),
            Ok(Event::CData(t)) => buf.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) => {
                let c = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c),
                    _ => match &*r {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => None,
                    },
                };
                if let Some(c) = c {
                    buf.push(c);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(value) = clean(&buf) {
                    apply(
                        &mut nfo,
                        &stack,
                        &value,
                        &mut uniqueid_type,
                        &mut actor,
                        &mut rating_value,
                    );
                }
                buf.clear();
                if let Some(name) = stack.pop() {
                    if name == "actor"
                        && let Some(a) = actor.take()
                        && !a.name.is_empty()
                    {
                        nfo.people.push(a);
                    }
                    if name == "rating" && stack.last().is_some_and(|p| p == "ratings") {
                        if (rating_default || nfo.rating.is_none())
                            && let Some(v) = rating_value
                        {
                            nfo.rating = Some(v);
                        }
                        rating_value = None;
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => {
                // Truncated or broken files: keep what was read, including
                // the text of the element being read.
                if let Some(value) = clean(&buf) {
                    apply(
                        &mut nfo,
                        &stack,
                        &value,
                        &mut uniqueid_type,
                        &mut actor,
                        &mut rating_value,
                    );
                }
                break;
            }
            _ => {}
        }
    }
    nfo
}

fn apply(
    nfo: &mut Nfo,
    stack: &[String],
    value: &str,
    uniqueid_type: &mut Option<String>,
    actor: &mut Option<Person>,
    rating_value: &mut Option<f64>,
) {
    let Some(name) = stack.last().map(String::as_str) else {
        return;
    };
    // Fields of nested elements.
    if stack.len() == 3 && stack[1] == "actor" {
        if let Some(a) = actor.as_mut() {
            match name {
                "name" => a.name = value.to_owned(),
                "role" => a.character = Some(value.to_owned()),
                _ => {}
            }
        }
        return;
    }
    if stack.len() >= 3 && stack[stack.len() - 2] == "rating" && name == "value" {
        *rating_value = value.parse().ok();
        return;
    }
    if stack.len() != 2 {
        return;
    }
    let v = || Some(value.to_owned());
    match name {
        "title" => nfo.title = v(),
        "originaltitle" => nfo.original_title = v(),
        "sorttitle" => nfo.sort_title = v(),
        "year" => nfo.year = value.parse().ok().filter(|y| *y > 0),
        "premiered" | "aired" | "releasedate" => nfo.premiered = Some(value.chars().take(10).collect()),
        "plot" => nfo.plot = v(),
        "outline" if nfo.plot.is_none() => nfo.plot = v(),
        "tagline" => nfo.tagline = v(),
        "rating" => nfo.rating = value.parse().ok(),
        "mpaa" | "certification" => nfo.mpaa = Some(value.trim_start_matches("Rated ").to_owned()),
        "genre" => nfo.genres.extend(
            value
                .split(" / ")
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(String::from),
        ),
        "studio" => nfo.studios.push(value.to_owned()),
        "director" | "credits" => nfo.people.push(Person {
            name: value.to_owned(),
            role: if name == "director" { "director" } else { "writer" }.into(),
            character: None,
        }),
        "season" => nfo.season = value.parse().ok(),
        "episode" => nfo.episode = value.parse().ok(),
        "runtime" => nfo.runtime_minutes = value.parse().ok().filter(|m| *m > 0),
        "tmdbid" => nfo.tmdb = v(),
        "imdbid" | "imdb_id" => nfo.imdb = v(),
        "tvdbid" => nfo.tvdb = v(),
        "id" if value.starts_with("tt") => nfo.imdb = v(),
        "uniqueid" => match uniqueid_type.take().as_deref() {
            Some("tmdb") => nfo.tmdb = v(),
            Some("imdb") => nfo.imdb = v(),
            Some("tvdb") => nfo.tvdb = v(),
            _ => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_movie() {
        let nfo = parse(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<movie>
  <title>The Matrix</title>
  <originaltitle>The Matrix</originaltitle>
  <year>1999</year>
  <plot><![CDATA[A hacker learns the truth.]]></plot>
  <ratings>
    <rating name="imdb" max="10"><value>8.7</value></rating>
    <rating name="themoviedb" max="10" default="true"><value>8.2</value></rating>
  </ratings>
  <mpaa>Rated R</mpaa>
  <genre>Action</genre>
  <genre>Science Fiction</genre>
  <studio>Warner Bros.</studio>
  <director>Lana Wachowski</director>
  <uniqueid type="imdb" default="true">tt0133093</uniqueid>
  <uniqueid type="tmdb">603</uniqueid>
  <actor><name>Keanu Reeves</name><role>Neo</role><order>0</order></actor>
  <actor><name>Carrie-Anne Moss</name><role>Trinity</role></actor>
  <fileinfo><streamdetails><video><codec>h264</codec></video></streamdetails></fileinfo>
</movie>"#,
        );
        assert_eq!(nfo.title.as_deref(), Some("The Matrix"));
        assert_eq!(nfo.year, Some(1999));
        assert_eq!(nfo.plot.as_deref(), Some("A hacker learns the truth."));
        assert_eq!(nfo.rating, Some(8.2));
        assert_eq!(nfo.mpaa.as_deref(), Some("R"));
        assert_eq!(nfo.genres, ["Action", "Science Fiction"]);
        assert_eq!(nfo.tmdb.as_deref(), Some("603"));
        assert_eq!(nfo.imdb.as_deref(), Some("tt0133093"));
        assert_eq!(nfo.people.len(), 3);
        let neo = nfo.people.iter().find(|p| p.name == "Keanu Reeves").unwrap();
        assert_eq!(neo.character.as_deref(), Some("Neo"));
    }

    #[test]
    fn parses_an_episode_and_ignores_garbage() {
        let nfo = parse(
            "<episodedetails><title>Pilot</title><season>1</season><episode>1</episode><aired>2008-01-20</aired></episodedetails>",
        );
        assert_eq!((nfo.season, nfo.episode), (Some(1), Some(1)));
        assert_eq!(nfo.premiered.as_deref(), Some("2008-01-20"));
        assert_eq!(parse("https://www.themoviedb.org/movie/603"), Nfo::default());
        assert_eq!(
            parse("<movie><title>Broken"),
            Nfo {
                title: Some("Broken".into()),
                ..Default::default()
            }
        );
    }
}
