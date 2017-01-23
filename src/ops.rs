use md6;
use std::iter;
use time::now;
use unicase::UniCase;
use iron::mime::Mime;
use std::sync::RwLock;
use lazysort::SortedBy;
use std::path::PathBuf;
use std::fs::{self, File};
use std::default::Default;
use iron::modifiers::Header;
use std::collections::HashMap;
use self::super::{Options, Error};
use mime_guess::guess_mime_type_opt;
use std::io::{self, Read, Seek, SeekFrom};
use trivial_colours::{Reset as CReset, Colour as C};
use iron::{headers, status, method, mime, IronResult, Listening, Response, TypeMap, Request, Handler, Iron};
use self::super::util::{url_path, file_hash, is_symlink, encode_str, encode_file, hash_string, html_response, file_binary, percent_decode, response_encoding,
                        detect_file_as_dir, encoding_extension, file_time_modified, human_readable_size, USER_AGENT, ERROR_HTML, INDEX_EXTENSIONS,
                        MIN_ENCODING_GAIN, MAX_ENCODING_SIZE, MIN_ENCODING_SIZE, DIRECTORY_LISTING_HTML, BLACKLISTED_ENCODING_EXTENSIONS};


macro_rules! log {
    ($fmt:expr) => {
        print!("{}[{}]{} ", C::Cyan, now().strftime("%F %T").unwrap(), CReset);
        println!($fmt);
    };
    ($fmt:expr, $($arg:tt)*) => {
        print!("{}[{}]{} ", C::Cyan, now().strftime("%F %T").unwrap(), CReset);
        println!($fmt, $($arg)*);
    };
}


// TODO: ideally this String here would be Encoding instead but hyper is bad
type CacheT<Cnt> = HashMap<([u8; 32], String), Cnt>;

pub struct HttpHandler {
    pub hosted_directory: (String, PathBuf),
    pub follow_symlinks: bool,
    pub check_indices: bool,
    pub writes_temp_dir: Option<(String, PathBuf)>,
    pub encoded_temp_dir: Option<(String, PathBuf)>,
    cache_gen: RwLock<CacheT<Vec<u8>>>,
    cache_fs: RwLock<CacheT<(PathBuf, bool)>>,
}

impl HttpHandler {
    pub fn new(opts: &Options) -> HttpHandler {
        HttpHandler {
            hosted_directory: opts.hosted_directory.clone(),
            follow_symlinks: opts.follow_symlinks,
            check_indices: opts.check_indices,
            writes_temp_dir: HttpHandler::temp_subdir(&opts.temp_directory, opts.allow_writes, "writes"),
            encoded_temp_dir: HttpHandler::temp_subdir(&opts.temp_directory, opts.encode_fs, "encoded"),
            cache_gen: Default::default(),
            cache_fs: Default::default(),
        }
    }

    fn temp_subdir(td: &Option<(String, PathBuf)>, flag: bool, name: &str) -> Option<(String, PathBuf)> {
        if flag && td.is_some() {
            let &(ref temp_name, ref temp_dir) = td.as_ref().unwrap();
            Some((format!("{}{}{}",
                          temp_name,
                          if temp_name.ends_with("/") || temp_name.ends_with(r"\") {
                              ""
                          } else {
                              "/"
                          },
                          name),
                  temp_dir.join(name)))
        } else {
            None
        }
    }
}

impl Handler for HttpHandler {
    fn handle(&self, req: &mut Request) -> IronResult<Response> {
        match req.method {
            method::Options => self.handle_options(req),
            method::Get => self.handle_get(req),
            method::Put => self.handle_put(req),
            method::Delete => self.handle_delete(req),
            method::Head => {
                self.handle_get(req).map(|mut r| {
                    r.body = None;
                    r
                })
            }
            method::Trace => self.handle_trace(req),
            _ => self.handle_bad_method(req),
        }
    }
}

impl HttpHandler {
    fn handle_options(&self, req: &mut Request) -> IronResult<Response> {
        log!("{}{}{} asked for {}OPTIONS{}", C::Green, req.remote_addr, CReset, C::Red, CReset);
        Ok(Response::with((status::NoContent,
                           Header(headers::Server(USER_AGENT.to_string())),
                           Header(headers::Allow(vec![method::Options, method::Get, method::Put, method::Delete, method::Head, method::Trace])))))
    }

    fn handle_get(&self, req: &mut Request) -> IronResult<Response> {
        let (req_p, symlink, url_err) = self.parse_requested_path(req);
        let file = req_p.is_file();
        let range = req.headers.get().map(|ref r: &headers::Range| (*r).clone());

        if url_err {
            self.handle_invalid_url(req, "<p>Percent-encoding decoded to invalid UTF-8.</p>")
        } else if !req_p.exists() || (symlink && !self.follow_symlinks) {
            self.handle_nonexistant(req, req_p)
        } else if file && range.is_some() {
            self.handle_get_file_range(req, req_p, range.unwrap())
        } else if file {
            self.handle_get_file(req, req_p)
        } else {
            self.handle_get_dir(req, req_p)
        }
    }

    fn handle_invalid_url(&self, req: &mut Request, cause: &str) -> IronResult<Response> {
        log!("{}{}{} requested to {}{}{} {}{}{} with invalid URL -- {}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             req.method,
             CReset,
             C::Yellow,
             req.url,
             CReset,
             cause.replace("<p>", "").replace("</p>", ""));


        self.handle_generated_response_encoding(req,
                                                status::BadRequest,
                                                html_response(ERROR_HTML, &["400 Bad Request", "The request URL was invalid.", cause]))
    }

    fn handle_nonexistant(&self, req: &mut Request, req_p: PathBuf) -> IronResult<Response> {
        log!("{}{}{} requested to {}{}{} nonexistant entity {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             req.method,
             CReset,
             C::Magenta,
             req_p.display(),
             CReset);
        let url_p = url_path(&req.url);
        self.handle_generated_response_encoding(req,
                                                status::NotFound,
                                                html_response(ERROR_HTML,
                                                              &["404 Not Found", &format!("The requested entity \"{}\" doesn't exist.", url_p), ""]))
    }

    fn handle_get_file_range(&self, req: &mut Request, req_p: PathBuf, range: headers::Range) -> IronResult<Response> {
        match range {
            headers::Range::Bytes(ref brs) => {
                if brs.len() == 1 {
                    let flen = req_p.metadata().unwrap().len();
                    match brs[0] {
                        // Cases where from is bigger than to are filtered out by iron so can never happen
                        headers::ByteRangeSpec::FromTo(from, to) => self.handle_get_file_closed_range(req, req_p, from, to),
                        headers::ByteRangeSpec::AllFrom(from) => {
                            if flen < from {
                                self.handle_get_file_empty_range(req, req_p, from, flen)
                            } else {
                                self.handle_get_file_right_opened_range(req, req_p, from)
                            }
                        }
                        headers::ByteRangeSpec::Last(from) => {
                            if flen < from {
                                self.handle_get_file_empty_range(req, req_p, from, flen)
                            } else {
                                self.handle_get_file_left_opened_range(req, req_p, from)
                            }
                        }
                    }
                } else {
                    self.handle_invalid_range(req, req_p, &range, "More than one range is unsupported.")
                }
            }
            headers::Range::Unregistered(..) => self.handle_invalid_range(req, req_p, &range, "Custom ranges are unsupported."),
        }
    }

    fn handle_get_file_closed_range(&self, req: &mut Request, req_p: PathBuf, from: u64, to: u64) -> IronResult<Response> {
        let mime_type = guess_mime_type_opt(&req_p).unwrap_or_else(|| if file_binary(&req_p) {
            "application/octet-stream".parse().unwrap()
        } else {
            "text/plain".parse().unwrap()
        });
        log!("{}{}{} was served byte range {}-{} of file {}{}{} as {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             from,
             to,
             C::Magenta,
             req_p.display(),
             CReset,
             C::Blue,
             mime_type,
             CReset);

        let mut buf = vec![0; (to + 1 - from) as usize];
        let mut f = File::open(&req_p).unwrap();
        f.seek(SeekFrom::Start(from)).unwrap();
        f.read(&mut buf).unwrap();

        Ok(Response::with((status::PartialContent,
                           (Header(headers::Server(USER_AGENT.to_string())),
                            Header(headers::LastModified(headers::HttpDate(file_time_modified(&req_p)))),
                            Header(headers::ContentRange(headers::ContentRangeSpec::Bytes {
                                range: Some((from, to)),
                                instance_length: Some(f.metadata().unwrap().len()),
                            })),
                            Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes]))),
                           buf,
                           mime_type)))
    }

    fn handle_get_file_right_opened_range(&self, req: &mut Request, req_p: PathBuf, from: u64) -> IronResult<Response> {
        let mime_type = guess_mime_type_opt(&req_p).unwrap_or_else(|| if file_binary(&req_p) {
            "application/octet-stream".parse().unwrap()
        } else {
            "text/plain".parse().unwrap()
        });
        log!("{}{}{} was served file {}{}{} from byte {} as {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Magenta,
             req_p.display(),
             CReset,
             from,
             C::Blue,
             mime_type,
             CReset);

        let flen = req_p.metadata().unwrap().len();
        self.handle_get_file_opened_range(req_p, SeekFrom::Start(from), from, flen - from, mime_type)
    }

    fn handle_get_file_left_opened_range(&self, req: &mut Request, req_p: PathBuf, from: u64) -> IronResult<Response> {
        let mime_type = guess_mime_type_opt(&req_p).unwrap_or_else(|| if file_binary(&req_p) {
            "application/octet-stream".parse().unwrap()
        } else {
            "text/plain".parse().unwrap()
        });
        log!("{}{}{} was served last {} bytes of file {}{}{} as {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             from,
             C::Magenta,
             req_p.display(),
             CReset,
             C::Blue,
             mime_type,
             CReset);

        let flen = req_p.metadata().unwrap().len();
        self.handle_get_file_opened_range(req_p, SeekFrom::End(-(from as i64)), flen - from, from, mime_type)
    }

    fn handle_get_file_opened_range(&self, req_p: PathBuf, s: SeekFrom, b_from: u64, clen: u64, mt: Mime) -> IronResult<Response> {
        let mut f = File::open(&req_p).unwrap();
        let flen = f.metadata().unwrap().len();
        f.seek(s).unwrap();

        Ok(Response::with((status::PartialContent,
                           f,
                           (Header(headers::Server(USER_AGENT.to_string())),
                            Header(headers::LastModified(headers::HttpDate(file_time_modified(&req_p)))),
                            Header(headers::ContentRange(headers::ContentRangeSpec::Bytes {
                                range: Some((b_from, flen - 1)),
                                instance_length: Some(flen),
                            })),
                            Header(headers::ContentLength(clen)),
                            Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes]))),
                           mt)))
    }

    fn handle_invalid_range(&self, req: &mut Request, req_p: PathBuf, range: &headers::Range, reason: &str) -> IronResult<Response> {
        self.handle_generated_response_encoding(req,
                                                status::RangeNotSatisfiable,
                                                html_response(ERROR_HTML,
                                                              &["416 Range Not Satisfiable",
                                                                &format!("Requested range <samp>{}</samp> could not be fullfilled for file {}.",
                                                                         range,
                                                                         req_p.display()),
                                                                reason]))
    }

    fn handle_get_file_empty_range(&self, req: &mut Request, req_p: PathBuf, from: u64, to: u64) -> IronResult<Response> {
        let mime_type = guess_mime_type_opt(&req_p).unwrap_or_else(|| if file_binary(&req_p) {
            "application/octet-stream".parse().unwrap()
        } else {
            "text/plain".parse().unwrap()
        });
        log!("{}{}{} was served an empty range from file {}{}{} as {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Magenta,
             req_p.display(),
             CReset,
             C::Blue,
             mime_type,
             CReset);

        Ok(Response::with((status::NoContent,
                           Header(headers::Server(USER_AGENT.to_string())),
                           Header(headers::LastModified(headers::HttpDate(file_time_modified(&req_p)))),
                           Header(headers::ContentRange(headers::ContentRangeSpec::Bytes {
                               range: Some((from, to)),
                               instance_length: Some(req_p.metadata().unwrap().len()),
                           })),
                           Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes])),
                           mime_type)))
    }

    fn handle_get_file(&self, req: &mut Request, req_p: PathBuf) -> IronResult<Response> {
        let mime_type = guess_mime_type_opt(&req_p).unwrap_or_else(|| if file_binary(&req_p) {
            "application/octet-stream".parse().unwrap()
        } else {
            "text/plain".parse().unwrap()
        });
        log!("{}{}{} was served file {}{}{} as {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Magenta,
             req_p.display(),
             CReset,
             C::Blue,
             mime_type,
             CReset);

        let flen = req_p.metadata().unwrap().len();
        if self.encoded_temp_dir.is_some() && flen > MIN_ENCODING_SIZE && flen < MAX_ENCODING_SIZE &&
           req_p.extension().and_then(|s| s.to_str()).map(|s| !BLACKLISTED_ENCODING_EXTENSIONS.contains(&UniCase(s))).unwrap_or(true) {
            self.handle_get_file_encoded(req, req_p, mime_type)
        } else {
            Ok(Response::with((status::Ok,
                               Header(headers::Server(USER_AGENT.to_string())),
                               Header(headers::LastModified(headers::HttpDate(file_time_modified(&req_p)))),
                               Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes])),
                               req_p,
                               mime_type)))
        }
    }

    fn handle_get_file_encoded(&self, req: &mut Request, req_p: PathBuf, mt: Mime) -> IronResult<Response> {
        if let Some(encoding) = req.headers.get_mut::<headers::AcceptEncoding>().and_then(|es| response_encoding(&mut **es)) {
            self.create_temp_dir(&self.encoded_temp_dir);
            let cache_key = (file_hash(&req_p), encoding.to_string());

            {
                match self.cache_fs.read().unwrap().get(&cache_key) {
                    Some(&(ref resp_p, true)) => {
                        log!("{} encoded as {} for {:.1}% ratio (cached)",
                             iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                             encoding,
                             ((req_p.metadata().unwrap().len() as f64) / (resp_p.metadata().unwrap().len() as f64)) * 100f64);

                        return Ok(Response::with((status::Ok,
                                                  Header(headers::Server(USER_AGENT.to_string())),
                                                  Header(headers::ContentEncoding(vec![encoding])),
                                                  Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes])),
                                                  resp_p.as_path(),
                                                  mt)));
                    }
                    Some(&(ref resp_p, false)) => {
                        return Ok(Response::with((status::Ok,
                                                  Header(headers::Server(USER_AGENT.to_string())),
                                                  Header(headers::LastModified(headers::HttpDate(file_time_modified(&resp_p)))),
                                                  Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes])),
                                                  resp_p.as_path(),
                                                  mt)));
                    }
                    None => (),
                }
            }

            let mut resp_p = self.encoded_temp_dir.as_ref().unwrap().1.join(hash_string(&cache_key.0));
            match (req_p.extension(), encoding_extension(&encoding)) {
                (Some(ext), Some(enc)) => resp_p.set_extension(format!("{}.{}", ext.to_str().unwrap_or("ext"), enc)),
                (Some(ext), None) => resp_p.set_extension(format!("{}.{}", ext.to_str().unwrap_or("ext"), encoding)),
                (None, Some(enc)) => resp_p.set_extension(enc),
                (None, None) => resp_p.set_extension(format!("{}", encoding)),
            };

            if encode_file(&req_p, &resp_p, &encoding) {
                let gain = (req_p.metadata().unwrap().len() as f64) / (resp_p.metadata().unwrap().len() as f64);
                if gain < MIN_ENCODING_GAIN {
                    let mut cache = self.cache_fs.write().unwrap();
                    cache.insert(cache_key, (req_p.clone(), false));
                    fs::remove_file(resp_p).unwrap();
                } else {
                    log!("{} encoded as {} for {:.1}% ratio",
                         iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                         encoding,
                         gain * 100f64);

                    let mut cache = self.cache_fs.write().unwrap();
                    cache.insert(cache_key, (resp_p.clone(), true));

                    return Ok(Response::with((status::Ok,
                                              Header(headers::Server(USER_AGENT.to_string())),
                                              Header(headers::ContentEncoding(vec![encoding])),
                                              Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes])),
                                              resp_p.as_path(),
                                              mt)));
                }
            } else {
                log!("{} failed to encode as {}, sending identity",
                     iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                     encoding);
            }
        }

        Ok(Response::with((status::Ok,
                           Header(headers::Server(USER_AGENT.to_string())),
                           Header(headers::LastModified(headers::HttpDate(file_time_modified(&req_p)))),
                           Header(headers::AcceptRanges(vec![headers::RangeUnit::Bytes])),
                           req_p,
                           mt)))
    }

    fn handle_get_dir(&self, req: &mut Request, req_p: PathBuf) -> IronResult<Response> {
        if self.check_indices {
            let mut idx = req_p.join("index");
            if let Some(e) = INDEX_EXTENSIONS.iter()
                .find(|e| {
                    idx.set_extension(e);
                    idx.exists()
                }) {
                if req.url.path().pop() == Some("") {
                    let r = self.handle_get_file(req, idx);
                    log!("{} found index file for directory {}{}{}",
                         iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                         C::Magenta,
                         req_p.display(),
                         CReset);
                    return r;
                } else {
                    return self.handle_get_dir_index_no_slash(req, e);
                }
            }
        }

        self.handle_get_dir_listing(req, req_p)
    }

    fn handle_get_dir_index_no_slash(&self, req: &mut Request, idx_ext: &str) -> IronResult<Response> {
        let new_url = req.url.to_string() + "/";
        log!("Redirecting {}{}{} to {}{}{} - found index file {}index.{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Yellow,
             new_url,
             CReset,
             C::Magenta,
             idx_ext,
             CReset);

        // We redirect here because if we don't and serve the index right away funky shit happens.
        // Example:
        //   - Without following slash:
        //     https://cloud.githubusercontent.com/assets/6709544/21442017/9eb20d64-c89b-11e6-8c7b-888b5f70a403.png
        //   - With following slash:
        //     https://cloud.githubusercontent.com/assets/6709544/21442028/a50918c4-c89b-11e6-8936-c29896947f6a.png
        Ok(Response::with((status::MovedPermanently, Header(headers::Server(USER_AGENT.to_string())), Header(headers::Location(new_url)))))
    }

    fn handle_get_dir_listing(&self, req: &mut Request, req_p: PathBuf) -> IronResult<Response> {
        let relpath = (url_path(&req.url) + "/").replace("//", "/");
        let is_root = &req.url.path() == &[""];
        log!("{}{}{} was served directory listing for {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Magenta,
             req_p.display(),
             CReset);
        self.handle_generated_response_encoding(req,
                                                status::Ok,
                                                html_response(DIRECTORY_LISTING_HTML,
                                                              &[&relpath,
                                                                &if self.writes_temp_dir.is_some() {
                                                                    r#"<script type="text/javascript">{drag_drop}</script>"#.to_string()
                                                                } else {
                                                                    String::new()
                                                                },
                                                                &if is_root {
                                                                    String::new()
                                                                } else {
                                                                    let rel_noslash = &relpath[0..relpath.len() - 1];
                                                                    let slash_idx = rel_noslash.rfind('/');
                                                                    format!("<tr><td><a href=\"/{}{}\"><img id=\"parent_dir\" \
                                                                             src=\"{{back_arrow_icon}}\" /></a></td> <td><a href=\"/{0}{1}\">Parent \
                                                                             directory</a></td> <td>{}</td> <td></td></tr>",
                                                                            slash_idx.map(|i| &rel_noslash[0..i]).unwrap_or(""),
                                                                            if slash_idx.is_some() { "/" } else { "" },
                                                                            file_time_modified(req_p.parent().unwrap()).strftime("%F %T").unwrap())
                                                                },
                                                                &req_p.read_dir()
                                                                    .unwrap()
                                                                    .map(Result::unwrap)
                                                                    .filter(|f| self.follow_symlinks || !is_symlink(f.path()))
                                                                    .sorted_by(|lhs, rhs| {
                                                                        (lhs.file_type().unwrap().is_file(), lhs.file_name().to_str().unwrap().to_lowercase())
                                                                            .cmp(&(rhs.file_type().unwrap().is_file(),
                                                                                   rhs.file_name().to_str().unwrap().to_lowercase()))
                                                                    })
                                                                    .fold("".to_string(), |cur, f| {
                let is_file = f.file_type().unwrap().is_file();
                let path = f.path();
                let fname = f.file_name().into_string().unwrap();
                let len = f.metadata().unwrap().len();
                let mime = if is_file {
                    match guess_mime_type_opt(&path) {
                        Some(mime::Mime(mime::TopLevel::Image, ..)) |
                        Some(mime::Mime(mime::TopLevel::Video, ..)) => "_image",
                        Some(mime::Mime(mime::TopLevel::Text, ..)) => "_text",
                        Some(mime::Mime(mime::TopLevel::Application, ..)) => "_binary",
                        None => if file_binary(&path) { "" } else { "_text" },
                        _ => "",
                    }
                } else {
                    ""
                };

                format!("{}<tr><td><a href=\"{}{}\"><img id=\"{}\" src=\"{{{}{}_icon}}\" /></a></td> <td><a href=\"{1}{2}\">{2}{}</a></td> <td>{}</td> \
                         <td>{}{}{}{}{}</td></tr>\n",
                        cur,
                        format!("/{}", relpath).replace("//", "/"),
                        fname,
                        path.file_name().map(|p| p.to_str().unwrap().replace('.', "_")).as_ref().unwrap_or(&fname),
                        if is_file { "file" } else { "dir" },
                        mime,
                        if is_file { "" } else { "/" },
                        file_time_modified(&path).strftime("%F %T").unwrap(),
                        if is_file { "<abbr title=\"" } else { "" },
                        if is_file {
                            len.to_string()
                        } else {
                            String::new()
                        },
                        if is_file { " B\">" } else { "" },
                        if is_file {
                            human_readable_size(len)
                        } else {
                            String::new()
                        },
                        if is_file { "</abbr>" } else { "" })
            })]))
    }

    fn handle_put(&self, req: &mut Request) -> IronResult<Response> {
        if self.writes_temp_dir.is_none() {
            return self.handle_forbidden_method(req, "-w", "write requests");
        }

        let (req_p, _, url_err) = self.parse_requested_path(req);

        if url_err {
            self.handle_invalid_url(req, "<p>Percent-encoding decoded to invalid UTF-8.</p>")
        } else if req_p.is_dir() {
            self.handle_disallowed_method(req, &[method::Options, method::Get, method::Delete, method::Head, method::Trace], "directory")
        } else if detect_file_as_dir(&req_p) {
            self.handle_invalid_url(req, "<p>Attempted to use file as directory.</p>")
        } else if req.headers.has::<headers::ContentRange>() {
            self.handle_put_partial_content(req)
        } else {
            self.create_temp_dir(&self.writes_temp_dir);
            self.handle_put_file(req, req_p)
        }
    }

    fn handle_disallowed_method(&self, req: &mut Request, allowed: &[method::Method], tpe: &str) -> IronResult<Response> {
        let allowed_s = allowed.iter()
            .enumerate()
            .fold("".to_string(), |cur, (i, m)| {
                cur + &m.to_string() +
                if i == allowed.len() - 2 {
                    ", and "
                } else if i == allowed.len() - 1 {
                    ""
                } else {
                    ", "
                }
            })
            .to_string();

        log!("{}{}{} tried to {}{}{} on {}{}{} ({}{}{}) but only {}{}{} are allowed",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             req.method,
             CReset,
             C::Magenta,
             url_path(&req.url),
             CReset,
             C::Blue,
             tpe,
             CReset,
             C::Red,
             allowed_s,
             CReset);

        let resp_text =
            html_response(ERROR_HTML,
                          &["405 Method Not Allowed", &format!("Can't {} on a {}.", req.method, tpe), &format!("<p>Allowed methods: {}</p>", allowed_s)]);
        self.handle_generated_response_encoding(req, status::MethodNotAllowed, resp_text)
            .map(|mut r| {
                r.headers.set(headers::Allow(allowed.to_vec()));
                r
            })
    }

    fn handle_put_partial_content(&self, req: &mut Request) -> IronResult<Response> {
        log!("{}{}{} tried to {}PUT{} partial content to {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             CReset,
             C::Yellow,
             url_path(&req.url),
             CReset);
        self.handle_generated_response_encoding(req,
                                                status::BadRequest,
                                                html_response(ERROR_HTML,
                                                              &["400 Bad Request",
                                                                "<a href=\"https://tools.ietf.org/html/rfc7231#section-4.3.3\">RFC7231 forbids \
                                                                 partial-content PUT requests.</a>",
                                                                ""]))
    }

    fn handle_put_file(&self, req: &mut Request, req_p: PathBuf) -> IronResult<Response> {
        let existant = req_p.exists();
        log!("{}{}{} {} {}{}{}, size: {}B",
             C::Green,
             req.remote_addr,
             CReset,
             if existant { "replaced" } else { "created" },
             C::Magenta,
             req_p.display(),
             CReset,
             *req.headers.get::<headers::ContentLength>().unwrap());

        let &(_, ref temp_dir) = self.writes_temp_dir.as_ref().unwrap();
        let temp_file_p = temp_dir.join(req_p.file_name().unwrap());

        io::copy(&mut req.body, &mut File::create(&temp_file_p).unwrap()).unwrap();
        let _ = fs::create_dir_all(req_p.parent().unwrap());
        fs::copy(&temp_file_p, req_p).unwrap();

        Ok(Response::with((if existant {
                               status::NoContent
                           } else {
                               status::Created
                           },
                           Header(headers::Server(USER_AGENT.to_string())))))
    }

    fn handle_delete(&self, req: &mut Request) -> IronResult<Response> {
        if self.writes_temp_dir.is_none() {
            return self.handle_forbidden_method(req, "-w", "write requests");
        }

        let (req_p, symlink, url_err) = self.parse_requested_path(req);

        if url_err {
            self.handle_invalid_url(req, "<p>Percent-encoding decoded to invalid UTF-8.</p>")
        } else if !req_p.exists() || (symlink && !self.follow_symlinks) {
            self.handle_nonexistant(req, req_p)
        } else {
            self.handle_delete_path(req, req_p)
        }
    }

    fn handle_delete_path(&self, req: &mut Request, req_p: PathBuf) -> IronResult<Response> {
        log!("{}{}{} deleted {}{} {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Blue,
             if req_p.is_file() { "file" } else { "directory" },
             C::Magenta,
             req_p.display(),
             CReset);

        if req_p.is_file() {
            fs::remove_file(req_p).unwrap();
        } else {
            fs::remove_dir_all(req_p).unwrap();
        }

        Ok(Response::with((status::NoContent, Header(headers::Server(USER_AGENT.to_string())))))
    }

    fn handle_trace(&self, req: &mut Request) -> IronResult<Response> {
        log!("{}{}{} requested {}TRACE{} for {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             CReset,
             C::Magenta,
             url_path(&req.url),
             CReset);

        let mut hdr = req.headers.clone();
        hdr.set(headers::ContentType("message/http".parse().unwrap()));

        Ok(Response {
            status: Some(status::Ok),
            headers: hdr,
            extensions: TypeMap::new(),
            body: None,
        })
    }

    fn handle_forbidden_method(&self, req: &mut Request, switch: &str, desc: &str) -> IronResult<Response> {
        log!("{}{}{} used disabled request method {}{}{} grouped under {}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             req.method,
             CReset,
             desc);
        self.handle_generated_response_encoding(req,
                                                status::Forbidden,
                                                html_response(ERROR_HTML,
                                                              &["403 Forbidden",
                                                                "This feature is currently disabled.",
                                                                &format!("<p>Ask the server administrator to pass <samp>{}</samp> to the executable to \
                                                                          enable support for {}.</p>",
                                                                         switch,
                                                                         desc)]))
    }

    fn handle_bad_method(&self, req: &mut Request) -> IronResult<Response> {
        log!("{}{}{} used invalid request method {}{}{}",
             C::Green,
             req.remote_addr,
             CReset,
             C::Red,
             req.method,
             CReset);
        let last_p = format!("<p>Unsupported request method: {}.<br />\nSupported methods: OPTIONS, GET, PUT, DELETE, HEAD and TRACE.</p>",
                             req.method);
        self.handle_generated_response_encoding(req,
                                                status::NotImplemented,
                                                html_response(ERROR_HTML, &["501 Not Implemented", "This operation was not implemented.", &last_p]))
    }

    fn handle_generated_response_encoding(&self, req: &mut Request, st: status::Status, resp: String) -> IronResult<Response> {
        if let Some(encoding) = req.headers.get_mut::<headers::AcceptEncoding>().and_then(|es| response_encoding(&mut **es)) {
            let mut cache_key = ([0u8; 32], encoding.to_string());
            md6::hash(256, resp.as_bytes(), &mut cache_key.0).unwrap();

            {
                if let Some(enc_resp) = self.cache_gen.read().unwrap().get(&cache_key) {
                    log!("{} encoded as {} for {:.1}% ratio (cached)",
                         iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                         encoding,
                         ((resp.len() as f64) / (enc_resp.len() as f64)) * 100f64);

                    return Ok(Response::with((st,
                                              Header(headers::Server(USER_AGENT.to_string())),
                                              Header(headers::ContentEncoding(vec![encoding])),
                                              "text/html;charset=utf-8".parse::<mime::Mime>().unwrap(),
                                              &enc_resp[..])));
                }
            }

            if let Some(enc_resp) = encode_str(&resp, &encoding) {
                log!("{} encoded as {} for {:.1}% ratio",
                     iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                     encoding,
                     ((resp.len() as f64) / (enc_resp.len() as f64)) * 100f64);

                let mut cache = self.cache_gen.write().unwrap();
                cache.insert(cache_key.clone(), enc_resp);

                return Ok(Response::with((st,
                                          Header(headers::Server(USER_AGENT.to_string())),
                                          Header(headers::ContentEncoding(vec![encoding])),
                                          "text/html;charset=utf-8".parse::<mime::Mime>().unwrap(),
                                          &cache[&cache_key][..])));
            } else {
                log!("{} failed to encode as {}, sending identity",
                     iter::repeat(' ').take(req.remote_addr.to_string().len()).collect::<String>(),
                     encoding);
            }
        }

        Ok(Response::with((st, Header(headers::Server(USER_AGENT.to_string())), "text/html;charset=utf-8".parse::<mime::Mime>().unwrap(), resp)))
    }

    fn parse_requested_path(&self, req: &Request) -> (PathBuf, bool, bool) {
        req.url.path().into_iter().filter(|p| !p.is_empty()).fold((self.hosted_directory.1.clone(), false, false), |(mut cur, mut sk, mut err), pp| {
            if let Some(pp) = percent_decode(pp) {
                cur.push(&*pp);
            } else {
                err = true;
            }

            while let Ok(newlink) = cur.read_link() {
                cur = newlink;
                sk = true;
            }

            (cur, sk, err)
        })
    }

    fn create_temp_dir(&self, td: &Option<(String, PathBuf)>) {
        let &(ref temp_name, ref temp_dir) = td.as_ref().unwrap();
        if !temp_dir.exists() && fs::create_dir_all(&temp_dir).is_ok() {
            log!("Created temp dir {}{}{}", C::Magenta, temp_name, CReset);
        }
    }
}

impl Clone for HttpHandler {
    fn clone(&self) -> HttpHandler {
        HttpHandler {
            hosted_directory: self.hosted_directory.clone(),
            follow_symlinks: self.follow_symlinks,
            check_indices: self.check_indices,
            writes_temp_dir: self.writes_temp_dir.clone(),
            encoded_temp_dir: self.encoded_temp_dir.clone(),
            cache_gen: Default::default(),
            cache_fs: Default::default(),
        }
    }
}


/// Attempt to start a server on ports from `from` to `up_to`, inclusive, with the specified handler.
///
/// If an error other than the port being full is encountered it is returned.
///
/// If all ports from the range are not free an error is returned.
///
/// # Examples
///
/// ```
/// # extern crate https;
/// # extern crate iron;
/// # use https::ops::try_ports;
/// # use iron::{status, Response};
/// let server = try_ports(|req| Ok(Response::with((status::Ok, "Abolish the burgeoisie!"))), 8000, 8100).unwrap();
/// ```
pub fn try_ports<H: Handler + Clone>(hndlr: H, from: u16, up_to: u16) -> Result<Listening, Error> {
    for port in from..up_to + 1 {
        match Iron::new(hndlr.clone()).http(("0.0.0.0", port)) {
            Ok(server) => return Ok(server),
            Err(error) => {
                if !error.to_string().contains("port") {
                    return Err(Error::Io {
                        desc: "server",
                        op: "start",
                        more: None,
                    });
                }
            }
        }
    }

    Err(Error::Io {
        desc: "server",
        op: "start",
        more: Some("no free ports"),
    })
}
