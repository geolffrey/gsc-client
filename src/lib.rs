#![recursion_limit = "128"]

use vlog::*;
use percent_encoding::{utf8_percent_encode, define_encode_set};
use std::cell::{Cell, RefCell};
use std::collections::{hash_map, HashMap};
use std::io;
use std::path::{Path, PathBuf};

pub mod cookie;
pub mod config;
pub mod errors;
pub mod messages;
pub mod table;

use self::errors::*;
use self::cookie::*;

pub struct GscClient {
    http:               reqwest::Client,
    config:             config::Config,
    submission_uris:    RefCell<HashMap<String, Vec<Option<String>>>>,
    had_warning:        Cell<bool>,
}

#[derive(Clone, Debug)]
pub struct RemotePattern {
    pub hw:     usize,
    pub pat:    String,
}

pub enum CpArg {
    Local(PathBuf),
    Remote(RemotePattern),
}

impl GscClient {
    pub fn new() -> Result<Self> {
        let mut config = config::Config::new();
        config.load_dotfile()?;

        Ok(GscClient {
            http:               reqwest::Client::new(),
            config,
            submission_uris:    RefCell::new(HashMap::new()),
            had_warning:        Cell::new(false),
        })
    }

    pub fn config(&self) -> &config::Config {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut config::Config {
        &mut self.config
    }

    pub fn had_warning(&self) -> bool {
        self.had_warning.get()
    }

    pub fn admin_csv(&self) -> Result<()> {
        let uri          = format!("{}/api/grades.csv", self.config.get_endpoint());
        let request      = self.http.get(&uri);
        let mut response = self.send_request(request)?;
        response.copy_to(&mut std::io::stdout())?;
        Ok(())
    }

    pub fn admin_divorce(&self, username: &str, hw: usize) -> Result<()> {
        let cookie      = self.load_cookie_file()?;
        let uri         = self.get_uri_for_submission(username, hw, cookie)?;
        let mut message = messages::SubmissionChange::default();
        message.owner2  = Some(());
        let mut request = self.http.patch(&uri);
        request.json(&message);
        let response    = self.send_request(request)?;
        self.print_results(response)
    }

    pub fn admin_extend(&self, username: &str, hw: usize, datetime: &str, eval: bool)
        -> Result<()> {

        let cookie       = self.load_cookie_file()?;
        let uri          = self.get_uri_for_submission(username, hw, cookie)?;
        let mut message  = messages::SubmissionChange::default();
        if eval {
            message.eval_date = Some(datetime.to_owned());
        } else {
            message.due_date  = Some(datetime.to_owned());
        }
        let mut request  = self.http.patch(&uri);
        request.json(&message);
        let response     = self.send_request(request)?;
        self.print_results(response)
    }

    pub fn admin_partners(&self, username: &str, hw: usize) -> Result<()> {
        let cookie       = self.load_cookie_file()?;
        let uri          = self.get_uri_for_submission(username, hw, cookie)?;
        let request      = self.http.get(&uri);
        let mut response = self.send_request(request)?;
        let submission: messages::Submission = response.json()?;

        let mut buf      = submission.owner1.name.clone();
        if let Some(owner2) = &submission.owner2 {
            buf.push(' ');
            buf += &owner2.name;
        }

        v1!("{}", buf);

        Ok(())
    }

    pub fn admin_set_exam(&self,
                          username: &str,
                          number: usize,
                          points: usize,
                          possible: usize) -> Result<()> {

        let uri         = self.user_uri(username);
        let mut message = messages::UserChange::default();
        message.exam_grades = vec![
            messages::ExamGrade { number, points, possible, }
        ];
        let mut request = self.http.patch(&uri);
        request.json(&message);
        let response    = self.send_request(request)?;
        self.print_results(response)
    }

    pub fn admin_submissions(&self, hw: usize) -> Result<()> {

        let uri         = format!("{}/api/submissions/hw{}", self.config.get_endpoint(), hw);
        let request     = self.http.get(&uri);
        let mut result  = self.send_request(request)?;
        let submissions: Vec<messages::SubmissionShort> = result.json()?;

        let mut table = table::TextTable::new(" %r  %l  %l\n");

        for submission in &submissions {
            table.add_row(table::Row::new()
                .add_cell(submission.id)
                .add_cell(format!("{}{}", self.config.get_endpoint(), submission.uri))
                .add_cell(submission.status));
        }

        v1!("{}", table);

        Ok(())
    }

    pub fn auth(&mut self, username: &str) -> Result<()> {
        let uri = self.user_uri(username);

        let cookie_file = self.config.get_cookie_file()?;

        loop {
            let password = prompt_password("Password", username)?;
            ve3!("> Sending request to {}", uri);
            let mut response = self.http.get(&uri)
                .basic_auth(username, Some(password))
                .send()?;

            let cookie_lock = CookieFile::new(cookie_file, username)?;
            match self.handle_response(&mut response, cookie_lock) {
                Ok(()) => {
                    v2!("Authenticated as {}", username);
                    return Ok(());
                }
                Err(e @ Error(ErrorKind::ServerError(JsonError { status: 401, .. }), _)) =>
                    eprintln!("{}", e),
                e =>
                    e?,
            }
        }
    }

    pub fn cp(&self, all: bool, srcs: &[CpArg], dst: &CpArg) -> Result<()> {
        match dst {
            CpArg::Local(filename) => self.cp_dn(all, srcs, filename),
            CpArg::Remote(rpat)    => self.cp_up(srcs, rpat),
        }
    }

    fn cp_dn(&self, all: bool, raw_srcs: &[CpArg], dst: &Path) -> Result<()> {
        let mut src_rpats = Vec::new();

        for src in raw_srcs {
            match src {
                CpArg::Local(filename) =>
                    Err(ErrorKind::CannotCopyLocalToLocal(filename.clone(), dst.to_owned()))?,
                CpArg::Remote(rpat)    =>
                    src_rpats.push(rpat),
            }
        }

        enum DstType {
            Dir,
            File,
            DoesNotExist,
        }

        let dst_type = match dst.metadata() {
            Err(e) =>
                match e.kind() {
                    io::ErrorKind::NotFound => DstType::DoesNotExist,
                    _                       => Err(e)?,
                }
            Ok(metadata) =>
                if metadata.is_dir() {
                    DstType::Dir
                } else {
                    DstType::File
                }
        };

        let mut src_files = Vec::new();

        for src_rpat in &src_rpats {
            let whole_hw = src_rpat.pat.is_empty();
            src_files.extend(
                self.fetch_file_list(src_rpat)?
                    .into_iter()
                    .map(|meta| {
                        let hw  = src_rpat.hw;
                        let pat = meta.name.clone();
                        (meta, whole_hw, RemotePattern { hw, pat })
                    }));
        }

        match dst_type {
            // cp FILE FILE
            DstType::File if src_files.len() == 1 =>
                if src_files[0].1 {
                    // cp -a hwN: FILE # error!
                    Err(ErrorKind::SourceDirToDestinationFile(src_files[0].2.hw,
                                                              dst.to_owned()))?;
                } else {
                    self.download_file(&src_files[0].2, &src_files[0].0.uri, dst)?;
                }

            // cp FILE FILE...+ FILE  # error!
            DstType::File =>
                Err(ErrorKind::MultipleSourcesOneDestination(dst.display().to_string()))?,

            // cp FILE FILE_DNE
            DstType::DoesNotExist if src_files.len() == 1 && !src_files[0].1 && !ends_in_slash(dst) =>
                self.download_file(&src_files[0].2, &src_files[0].0.uri, dst)?,

            // cp FILE... DIR
            // cp FILE... DIR_DNE
            _ => {
                // cp -a ...
                if all {
                    soft_create_dir(dst)?;
                }

                for (meta, whole_hw, rpat) in &src_files {
                    let mut file_dst = dst.to_owned();
                    if *whole_hw {
                        file_dst.push(meta.purpose.to_dir());
                        soft_create_dir(&file_dst)?;
                    }
                    file_dst.push(&meta.name);
                    self.download_file(rpat, &meta.uri, &file_dst)?;
                }
            }
        }

        v2!("Done.");
        Ok(())
    }

    fn download_file(&self, src: &RemotePattern, rel_uri: &str, dst: &Path)
        -> Result<()> {

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dst)?;

        let uri          = format!("{}{}", self.config.get_endpoint(), rel_uri);
        let request      = self.http.get(&uri);
        ve2!("Downloading ‘{}’ -> ‘{}’...", src, dst.display());
        let mut response = self.send_request(request)?;
        response.copy_to(&mut file)?;

        Ok(())
    }

    fn cp_up(&self, raw_srcs: &[CpArg], dst: &RemotePattern) -> Result<()> {
        let mut srcs = Vec::new();

        for src in raw_srcs {
            match src {
                CpArg::Local(filename) =>
                    srcs.push(filename),
                CpArg::Remote(rpat)    =>
                    Err(ErrorKind::CannotCopyRemoteToRemote(rpat.clone(), dst.clone()))?
            }
        }

        if dst.pat.is_empty() {
            for src in srcs {
                let filename     = match self.get_base_filename(&src) {
                    Ok(s)  => s,
                    Err(e) => {
                        ve1!("{}", e);
                        self.had_warning.set(true);
                        continue;
                    }
                };
                self.upload_file(src, &dst.with_pat(filename))?;
            }
        } else {
            let src = if srcs.len() == 1 {
                &srcs[0]
            } else {
                Err(ErrorKind::MultipleSourcesOneDestination(dst.to_string()))?
            };

            let dsts     = self.fetch_file_list(dst)?;
            let filename = match dsts.len() {
                0 => &dst.pat,
                1 => &dsts[0].name,
                _ => Err(dest_pat_is_multiple(dst, &dsts))?,
            };

            self.upload_file(src, &dst.with_pat(filename))?;
        }

        v2!("Done.");
        Ok(())
    }

    fn upload_file(&self, src: &Path, dst: &RemotePattern) -> Result<()> {
        let src_file     = std::fs::File::open(&src)?;
        let encoded_dst  = utf8_percent_encode(&dst.pat, ENCODE_SET);
        let base_uri     = self.get_uri_for_submission_files(dst.hw)?;
        let uri          = format!{"{}/{}", base_uri, encoded_dst};
        let mut request  = self.http.put(&uri);
        request.body(src_file);
        v2!("Uploading ‘{}’ -> ‘{}’...", src.display(), dst);
        self.send_request(request)?;

        Ok(())
    }

    fn get_base_filename<'a>(&self, path: &'a Path) -> Result<&'a str> {
        match path.file_name() {
            None         => Err(ErrorKind::BadLocalPath(path.to_owned()).into()),
            Some(os_str) => match os_str.to_str() {
                None         => Err(ErrorKind::FilenameNotUtf8(path.to_owned()).into()),
                Some(s)      => Ok(s),
            }
        }
    }

    pub fn deauth(&self) -> Result<()> {
        let uri          = format!("{}/api/whoami", self.config.get_endpoint());
        let request      = self.http.delete(&uri);
        let result       = match self.send_request(request) {
            Ok(mut response) => match response.json() {
                Ok(true)  => Ok("Deauthenticated with server."),
                Ok(false) => Err(format!("Could not deauthenticate with server.")),
                Err(e)    => Err(format!("Could not understand JSON from server:\n  {}", e)),
            }

            Err(e)    => match e.kind() {
                ErrorKind::LoginPlease => Ok("You aren’t authenticated."),
                _ => Err(format!("Could not deauthenticate with server:\n  {}", e)),
            }
        };

        match result {
            Ok(msg)  => v2!("{}", msg),
            Err(msg) => {
                ve1!("{}\nDeleting local credentials anyway.", msg);
                self.had_warning.set(true);
            }
        }

        let mut cookie = CookieFile::new(self.config.get_cookie_file()?, "")?;
        cookie.deauth();

        Ok(())
    }

    pub fn cat(&self, pats: &[RemotePattern]) -> Result<()> {
        for rpat in pats {
            let files = self.fetch_file_list(&rpat)?;

            if files.is_empty() {
                let error = Error::from(ErrorKind::NoSuchRemoteFile(rpat.clone()));
                ve1!("{}", error);
                self.had_warning.set(true);
            }

            for file in files {
                let uri          = format!("{}{}", self.config.get_endpoint(), file.uri);
                let request      = self.http.get(&uri);
                let mut response = self.send_request(request)?;
                response.copy_to(&mut std::io::stdout())?;
            }
        }

        Ok(())
    }

    pub fn create(&mut self, username: &str) -> Result<()> {
        let password = get_matching_passwords(username)?;
        let uri      = format!("{}/api/users", self.config.get_endpoint());

        ve3!("> Sending request to {}", uri);
        let mut response = self.http.post(&uri)
            .basic_auth(username, Some(password))
            .send()?;
        let cookie_lock = CookieFile::new(self.config.get_cookie_file()?, username)?;
        self.handle_response(&mut response, cookie_lock)?;

        v2!("Created account: {}.", username);

        Ok(())
    }

    pub fn ls(&self, rpat: &RemotePattern) -> Result<()> {

        let files     = self.fetch_file_list(&rpat)?;
        let mut table = table::TextTable::new("%r  %l  [%l] %l\n");

        if files.is_empty() {
            return Err(ErrorKind::NoSuchRemoteFile(rpat.clone()).into());
        }

        for file in &files {
            table.add_row(
                table::Row::new()
                    .add_cell(file.byte_count)
                    .add_cell(&file.upload_time)
                    .add_cell(file.purpose.to_char())
                    .add_cell(&file.name));
        }

        v1!("{}", table);

        Ok(())
    }

    pub fn partner(&self) -> Result<()> {
        let (user, cookie) = self.load_credentials()?;
        let uri            = self.user_uri(&user);
        let request        = self.http.get(&uri);
        let mut response   = self.send_request_with_cookie(request, cookie)?;
        let user: messages::User = response.json()?;
        self.print_partner_status(&user, "");
        Ok(())
    }

    pub fn partner_request(&self, hw: usize, them: &str) -> Result<()> {
        self.partner_operation(messages::PartnerRequestStatus::Outgoing, hw, them)
    }

    pub fn partner_accept(&self, hw: usize, them: &str)-> Result<()> {
        self.partner_operation(messages::PartnerRequestStatus::Accepted, hw, them)
    }

    pub fn partner_cancel(&self, hw: usize, them: &str)-> Result<()> {
        self.partner_operation(messages::PartnerRequestStatus::Canceled, hw, them)
    }

    fn partner_operation(&self,
                         op: messages::PartnerRequestStatus,
                         hw: usize,
                         them: &str)
        -> Result<()> {

        let (me, cookie) = self.load_credentials()?;
        let uri          = self.user_uri(&me);
        let mut message  = messages::UserChange::default();
        message.partner_requests = vec![
            messages::PartnerRequest {
                assignment_number:  hw,
                user:               them.to_owned(),
                status:             op,
            }
        ];

        let mut request = self.http.patch(&uri);
        request.json(&message);
        let response    = self.send_request_with_cookie(request, cookie)?;
        self.print_results(response)
    }

    pub fn passwd(&self) -> Result<()> {
        let (me, cookie) = self.load_credentials()?;
        let password     = get_matching_passwords(&me)?;
        let mut message  = messages::UserChange::default();
        message.password = Some(password);
        let uri          = self.user_uri(&me);
        let mut request  = self.http.patch(&uri);
        request.json(&message);
        let response     = self.send_request_with_cookie(request, cookie)?;
        self.print_results(response)
    }

    pub fn rm(&self, pats: &[RemotePattern]) -> Result<()> {
        for rpat in pats {
            let files = self.fetch_file_list(&rpat)?;

            if files.is_empty() {
                let error = Error::from(ErrorKind::NoSuchRemoteFile(rpat.clone()));
                ve1!("{}", error);
                self.had_warning.set(true);
            }

            for file in files {
                let uri          = format!("{}{}", self.config.get_endpoint(), file.uri);
                let request      = self.http.delete(&uri);
                v2!("Deleting remote file ‘hw{}:{}’...", rpat.hw, file.name);
                self.send_request(request)?;
            }
        }

        v2!("Done.");
        Ok(())
    }

    pub fn status_hw(&self, number: usize) -> Result<()>
    {
        let (me, cookie) = self.load_credentials()?;
        let uri          = self.get_uri_for_submission(&me, number, cookie)?;
        let request      = self.http.get(&uri);
        let mut response = self.send_request(request)?;

        let submission: messages::Submission = response.json()?;
        let in_evaluation   = submission.status.is_self_eval();
        let quota_remaining = submission.quota_remaining();

        let mut table = table::TextTable::new("  %l  %l\n");
        table.add_row(table::Row::new().add_cell("Submission status:")
            .add_cell(submission.status));

        if in_evaluation {
            table.add_row(table::Row::new().add_cell("Evaluation status:")
                .add_cell(submission.eval_status));
        }

        table
            .add_row(table::Row::new().add_cell("Open date:")
                .add_cell(submission.open_date))
            .add_row(table::Row::new().add_cell("Submission due date:")
                .add_cell(submission.due_date))
            .add_row(table::Row::new().add_cell("Self-eval due date:")
                .add_cell(submission.eval_date))
            .add_row(table::Row::new().add_cell("Last modified:")
                .add_cell(submission.last_modified))
            .add_row(table::Row::new().add_cell("Quota remaining:")
                .add_cell(format!("{:.1}% ({}/{} bytes used)",
                                  quota_remaining,
                                  submission.bytes_used,
                                  submission.bytes_quota)));

        let mut owners = submission.owner1.name.clone();
        if let Some(owner2) = &submission.owner2 {
            owners += " and ";
            owners += &owner2.name;
        }

        v1!("hw{} ({})", number, owners);
        v1!("{}", table);

        Ok(())
    }

    pub fn status_user(&self) -> Result<()> {
        let (me, cookie) = self.load_credentials()?;
        let uri          = self.user_uri(&me);
        let request      = self.http.get(&uri);
        let mut response = self.send_request_with_cookie(request, cookie)?;

        let user: messages::User = response.json()?;

        v1!("Status for {}:\n", user.name);

        if user.submissions.iter().any(|s| s.status != messages::SubmissionStatus::Future) {
            let mut table = table::TextTable::new("    hw%l: %r    %l\n");

            for s in &user.submissions {
                let grade = match s.status {
                    messages::SubmissionStatus::Future => continue,
                    messages::SubmissionStatus::Closed => format!("{:.1}%", s.grade),
                    _ => String::new(),
                };

                table.add_row(table::Row::new()
                    .add_cell(s.assignment_number)
                    .add_cell(grade)
                    .add_cell(s.status));
            }

            v1!("  Submissions:\n{}", table);
        }

        if !user.exam_grades.is_empty() {
            let mut table = table::TextTable::new("    ex%l: %r%%    (%l / %l)\n");

            for e in &user.exam_grades {
                let grade = format!("{:.1}", 100.0 * e.points as f64 / e.possible as f64);
                table.add_row(table::Row::new()
                    .add_cell(e.number)
                    .add_cell(grade)
                    .add_cell(e.points)
                    .add_cell(e.possible));
            }

            v1!("  Exam grades:\n{}", table);
        }

        if !user.partner_requests.is_empty() {
            self.print_partner_status(&user, "  ");
            v1!("Partner requests can be managed with the ‘gsc partner’ command.");
        }

        Ok(())
    }

    pub fn whoami(&self) -> Result<()> {
        let uri          = format!("{}/api/whoami", self.config.get_endpoint());
        let request      = self.http.get(&uri);
        let mut response = self.send_request(request)?;
        let text         = response.text()?;
        v1!("{}", text);
        Ok(())
    }

    // Helper methods

    fn fetch_file_list(&self, rpat: &RemotePattern) -> Result<Vec<messages::FileMeta>>
    {
        let matcher      = glob(&rpat.pat)?;
        let uri          = self.get_uri_for_submission_files(rpat.hw)?;
        let request      = self.http.get(&uri);
        let mut response = self.send_request(request)?;

        let files: Vec<messages::FileMeta> = response.json()?;

        Ok(files.into_iter()
            .filter(|file| matcher.is_match(&file.name))
            .collect())
    }

    fn fetch_submissions(&self, user: &str, cookie: CookieFile)
        -> Result<Vec<messages::SubmissionShort>> {

        let uri          = self.user_uri(user) + "/submissions";
        let request      = self.http.get(&uri);
        let mut response = self.send_request_with_cookie(request, cookie)?;
        response.json()
            .chain_err(|| "Could not understand response from server")
    }

    fn get_submission_uris(&self, user: &str, cookie: CookieFile) -> Result<Vec<Option<String>>> {
        let submissions = self.fetch_submissions(user, cookie)?;
        let mut result  = Vec::new();

        for submission in &submissions {
            let number = submission.assignment_number;

            while number >= result.len() {
                result.push(None);
            }

            result[number] = Some(format!("{}{}", self.config.get_endpoint(), submission.uri));
        }

        Ok(result)
    }

    fn get_uri_for_submission(&self, user: &str, number: usize, cookie: CookieFile)
        -> Result<String> {

        let mut cache = self.submission_uris.borrow_mut();
        let uris      = match cache.entry(user.to_owned()) {
            hash_map::Entry::Occupied(entry) =>
                entry.into_mut(),
            hash_map::Entry::Vacant(entry)   =>
                entry.insert(self.get_submission_uris(&user, cookie)?),
        };

        match uris.get(number) {
            Some(Some(uri)) => Ok(uri.to_owned()),
            _               => Err(ErrorKind::UnknownHomework(number).into()),
        }
    }

    fn get_uri_for_submission_files(&self, number: usize) -> Result<String> {
        let (user, cookie) = self.load_credentials()?;
        self.get_uri_for_submission(&user, number, cookie).map(|uri| uri + "/files")
    }

    fn handle_response(&self, response: &mut reqwest::Response, cookie_lock: CookieFile)
                       -> Result<()> {

        self.save_cookie(response, cookie_lock)?;

        if response.status().is_success() {
            Ok(())
        } else {
            let error = response.json()?;
            Err(ErrorKind::ServerError(error))?
        }
    }

    fn prepare_cookie(&self, request: &mut reqwest::RequestBuilder,
                      cookie_lock: &CookieFile)
        -> Result<()>
    {
        let cookie = cookie_lock.get_cookie_header();
        ve3!("> Sending cookie {}", cookie);
        request.header(cookie);
        Ok(())
    }

    fn print_partner_status(&self, user: &messages::User, indent: &str) {
        if user.partner_requests.is_empty() {
            ve1!("No outstanding partner requests.");
        } else {
            let mut table = table::TextTable::new("    %l %l\n");

            for p in &user.partner_requests {
                use self::messages::PartnerRequestStatus::*;
                let hw      = format!("hw{}:", p.assignment_number);
                let message = match p.status {
                    Outgoing => format!("sent to {}", p.user),
                    Incoming => format!("received from {}", p.user),
                    _        => continue,
                };
                table.add_row(table::Row::new()
                    .add_cell(hw)
                    .add_cell(message));
            }

            v1!("{}Partner requests:\n{}", indent, table);
        }
    }

    fn print_results(&self, mut response: reqwest::Response) -> Result<()> {
        let results: Vec<messages::JsonResult> = response.json()?;
        self.print_results_helper(&results);
        Ok(())
    }

    fn print_results_helper(&self, results: &[messages::JsonResult]) {
        for result in results {
            match result {
                messages::JsonResult::Success(msg) => {
                    v2!("{}", msg);
                }
                messages::JsonResult::Failure(msg) => {
                    ve1!("{}", msg);
                    self.had_warning.set(true);
                }
                messages::JsonResult::Nested(vec) => {
                    self.print_results_helper(&vec);
                }
            }
        }
    }

    fn user_uri(&self, user: &str) -> String {
        format!("{}/api/users/{}", self.config.get_endpoint(), user)
    }

    fn save_cookie(&self, response: &reqwest::Response, mut cookie_lock: CookieFile)
                   -> Result<()> {

        if let Some(reqwest::header::SetCookie(chunks)) = response.headers().get() {
            if let Some((key, value)) = parse_cookies(&chunks) {
                ve3!("< Received cookie {}={}", key, value);
                cookie_lock.set_cookie(key, value);
            }
        }

        Ok(())
    }

    fn load_cookie_file(&self) -> Result<CookieFile> {
        CookieFile::lock(self.config.get_cookie_file()?)
    }

    fn load_credentials(&self) -> Result<(String, CookieFile)> {
        let cookie_file = self.load_cookie_file()?;

        let user        = match self.config.get_on_behalf() {
            Some(s) => s,
            None    => cookie_file.get_username()
        }.to_owned();

        Ok((user, cookie_file))
    }

    fn send_request(&self, req_builder: reqwest::RequestBuilder)
        -> Result<reqwest::Response> {

        let cookie = self.load_cookie_file()?;
        self.send_request_with_cookie(req_builder, cookie)
    }

    fn send_request_with_cookie(&self, mut req_builder: reqwest::RequestBuilder,
                                cookie: CookieFile)
        -> Result<reqwest::Response> {

        self.prepare_cookie(&mut req_builder, &cookie)?;
        let request      = req_builder.build()?;
        ve3!("> Sending request to {}", request.url());
        let mut response = self.http.execute(request)?;
        self.handle_response(&mut response, cookie)?;
        Ok(response)
    }
}

define_encode_set! {
    pub ENCODE_SET = [percent_encoding::PATH_SEGMENT_ENCODE_SET] | { '+' }
}

fn ends_in_slash(path: &Path) -> bool {
    path.to_str().unwrap_or("").ends_with('/')
}

fn get_matching_passwords(username: &str) -> Result<String> {
    let password1 = prompt_password("New password", username)?;
    let password2 = prompt_password("Confirm password", username)?;

    if password1 == password2 {
        Ok(password1)
    } else {
        Err(errors::ErrorKind::PasswordMismatch)?
    }
}

fn glob(pattern: &str) -> Result<globset::GlobMatcher> {
    let real_pattern = if pattern.is_empty() { "*" } else { pattern };
    Ok(globset::Glob::new(real_pattern)?.compile_matcher())
}

pub fn parse_cookie(cookie: &str) -> Option<(String, String)> {
    let pair = match cookie.find(';') {
        Some(index) => &cookie[.. index],
        None        => cookie,
    };

    pair.find('=').map(|index| {
        let key   = pair[.. index].to_owned();
        let value = pair[index + 1 ..].to_owned();
        (key, value)
    })
}

fn parse_cookies(chunks: &[String]) -> Option<(String, String)> {
    for chunk in chunks {
        if let Some(pair) = parse_cookie(&chunk) {
            return Some(pair);
        }
    }

    None
}

fn prompt_password(prompt: &str, username: &str) -> Result<String> {
    let prompt   = format!("{} for {}: ", prompt, username);
    let password = rpassword::prompt_password_stderr(&prompt)?;
    Ok(password)
}

fn soft_create_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(_)  => Ok(()),
        Err(e) => match e.kind() {
            io::ErrorKind::AlreadyExists => Ok(()),
            _                            => Err(e)?,
        }
    }
}

impl RemotePattern {
    pub fn is_whole_hw(&self) -> bool {
        self.pat.is_empty()
    }

    pub fn with_pat(&self, pat: &str) -> Self {
        RemotePattern { hw: self.hw, pat: pat.to_owned(), }
    }
}

impl CpArg {
    pub fn is_whole_hw(&self) -> bool {
        match self {
            CpArg::Local(_)     => false,
            CpArg::Remote(rpat) => rpat.is_whole_hw(),
        }
    }
}

impl std::fmt::Display for RemotePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "hw{}:{}", self.hw, self.pat)
    }
}

