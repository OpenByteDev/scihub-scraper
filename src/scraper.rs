#[macro_use]
extern crate lazy_static;

use reqwest::{Client, header, redirect};
use scraper::{Html, Selector};
use url::Url;

pub struct SciHubScraper {
    client: Client,
    pub base_urls: Option<Vec<Url>>
}

impl Default for SciHubScraper {
    fn default() -> Self {
        Self::new()
    }
}

impl SciHubScraper {
    pub fn new() -> Self {
        SciHubScraper {
            client: Client::new(),
            base_urls: None
        }
    }
    /// Creates a new `SciHubScraper` with the given sci-hub base url. (This will disable the automatic sci-hub domain detection).
    pub fn with_base_url(base_url: Url) -> Self {
        Self::with_base_urls(vec![base_url])
    }
    /// Creates a new `SciHubScraper` with the given sci-hub base urls. (This will disable the automatic sci-hub domain detection).
    pub fn with_base_urls(base_urls: Vec<Url>) -> Self {
        SciHubScraper {
            client: Client::new(),
            base_urls: Some(base_urls)
        }
    }

    /// Fetches a list of base urls from sci-hub.now.sh.
    pub async fn fetch_base_urls(&mut self) -> Result<&Vec<Url>, Error> {
        let scihub_now_url = Url::parse("https://sci-hub.now.sh/").unwrap();
        self.fetch_base_urls_from_provider(scihub_now_url).await
    }
    /// Fetches a list of base urls from the given provider.
    pub async fn fetch_base_urls_from_provider(&mut self, scihub_url_provider: Url) -> Result<&Vec<Url>, Error> {
        let document = self.fetch_html_document(scihub_url_provider).await?;

        let link_selector = Selector::parse("a[href]").unwrap();
        let mut domains: Vec<Url> = document.select(&link_selector)
            .filter_map(|node| node.value().attr("href"))
            .filter_map(|href| Url::parse(href).ok())
            .filter(|url| url.domain().map_or(false, |e| e.starts_with("sci-hub") && !e.ends_with("now.sh")))
            .collect();
        domains.dedup();

        self.base_urls = Some(domains);
        Ok(self.base_urls.as_ref().unwrap())
    }
    async fn ensure_base_urls(&mut self) -> Result<&Vec<Url>, Error> {
        if self.base_urls.is_none() {
            self.fetch_base_urls().await?;
        }
        if let Some(vec) = &self.base_urls {
            if vec.is_empty() {
                return Err(Error::Other("No sci-hub domains found."));
            }
            Ok(&vec)
        } else {
            Err(Error::Other("Failed to load sci-hub domains."))
        }
    }

    /// Generates a scihub paper url from the given base url and doi.
    pub fn scihub_url_from_base_url_and_doi(base_url: &Url, doi: &str) -> Result<Url, url::ParseError> {
        base_url.join(doi)
    }

    /// Fetches the paper with the given doi from sci-hub, automatically fetching current sci-hub domains.
    pub async fn fetch_paper_by_doi(&mut self, doi: &str) -> Result<Paper, Error> {
        self.ensure_base_urls().await?;

        for base_url in self.base_urls.as_ref().unwrap() {
            let pdf_url = self.fetch_paper_by_base_url_and_doi(base_url, &doi).await?;
            return Ok(pdf_url);
        }
        Err(Error::Other("Invalid doi or no working sci-hub mirror found"))
    }
    /// Fetches the paper with the given url from sci-hub, automatically fetching current sci-hub domains.
    pub async fn fetch_paper_by_paper_url(&mut self, url: &str) -> Result<Paper, Error> {
        self.fetch_paper_by_doi(url).await
    }
    /// Fetches the paper with the given doi using the given sci-hub base url.
    pub async fn fetch_paper_by_base_url_and_doi(&self, base_url: &Url, doi: &str) -> Result<Paper, Error> {
        let url = Self::scihub_url_from_base_url_and_doi(base_url, doi)?;
        self.fetch_paper_from_scihub_url(url).await
    }
    /// Fetches the paper from the given scihub url.
    pub async fn fetch_paper_from_scihub_url(&self, url: Url) -> Result<Paper, Error> {
        let document = self.fetch_html_document(url.clone()).await?;

        lazy_static! {
            static ref TITLE_SELECTOR:Selector = Selector::parse("head title").unwrap();
            static ref DOWNLOAD_BUTTON_SELECTOR:Selector = Selector::parse("#buttons a[onclick]").unwrap();
            static ref VERSIONS_SELECTOR:Selector = Selector::parse("#versions a[href]").unwrap();
            static ref BOLD_SELECTOR:Selector = Selector::parse("b").unwrap();
        }

        let (doi, paper_title) = document.select(&TITLE_SELECTOR)
            .filter_map(|node| {
                let title = node.inner_html();
                let mut iter = title.rsplit("|").map(|e| e.trim());
                match (iter.next(), iter.next()) {
                    (Some(doi), Some(page_title)) => Some((String::from(doi), String::from(page_title))),
                    _ => None,
                }
            })
            .next()
            .ok_or(Error::SciHubParse("Paper info not found in page."))?;

        let raw_pdf_url = document.select(&DOWNLOAD_BUTTON_SELECTOR)
            .filter_map(|node| node.value().attr("onclick"))
            .filter_map(|attrval| Some(&attrval[attrval.find("'")?+1..attrval.rfind("'")?]))
            .next()
            .ok_or(Error::SciHubParse("Pdf url not found in page."))?;
        let pdf_url = convert_protocol_relative_url_to_absolute(raw_pdf_url, &url);

        let mut current_version = None;
        let other_versions: Vec<_> = document.select(&VERSIONS_SELECTOR)
            .filter_map(|node| {
                if current_version.is_none() {
                    if let Some(version_str) = node.select(&BOLD_SELECTOR).next().map(|b| b.inner_html()) {
                        current_version = Some(version_str);
                        return None; // do not include current version
                    }
                }

                let version_href = node.value().attr("href")?;
                let version_url = convert_protocol_relative_url_to_absolute(version_href, &url);

                Some(PaperVersion {
                    version: node.inner_html(),
                    scihub_url: String::from(version_url)
                })
            })
            .collect();
        
        let current_version = current_version.unwrap_or(String::from("current"));

        Ok(Paper {
            scihub_url: url,
            doi: doi,
            title: paper_title,
            version: current_version,
            download_url: pdf_url,
            other_versions: other_versions
        })
    }

    /// Fetches the pdf url of the paper with the given doi from sci-hub, automatically fetching current sci-hub domains.
    pub async fn fetch_paper_pdf_url_by_doi(&mut self, doi: &str) -> Result<String, Error> {
        self.ensure_base_urls().await?;

        for base_url in self.base_urls.as_ref().unwrap() {
            let pdf_url = self.fetch_paper_pdf_url_by_base_url_and_doi(base_url, &doi).await?;
            return Ok(pdf_url);
        }
        Err(Error::Other("Invalid doi or no working sci-hub mirror found"))
    }
    /// Fetches the pdf url of the paper with the given url from sci-hub, automatically fetching current sci-hub domains.
    pub async fn fetch_paper_pdf_url_by_paper_url(&mut self, url: &str) -> Result<String, Error> {
        self.fetch_paper_pdf_url_by_doi(url).await
    }
    /// Fetches the pdf url of the paper with the given doi using the given sci-hub base url.
    pub async fn fetch_paper_pdf_url_by_base_url_and_doi(&self, base_url: &Url, doi: &str) -> Result<String, Error> {
        let url = Self::scihub_url_from_base_url_and_doi(base_url, doi)?;
        self.fetch_paper_pdf_url_from_scihub_url(url).await
    }
    /// Fetches the pdf url of the paper from the given scihub url.
    pub async fn fetch_paper_pdf_url_from_scihub_url(&self, url: Url) -> Result<String, Error> {
        let client = Client::builder()
            .redirect(redirect::Policy::none())
            .build()?;
        
        let response = client.get(url.clone())
            .header(header::USER_AGENT, "Mozilla/5.0 (Android 4.4; Mobile; rv:42.0) Gecko/42.0 Firefox/42.0") // "disguise" as mobile
            .send().await?;
        
        response.headers()
            .get(header::LOCATION)
            .ok_or(Error::SciHubParse("Received unexpected response from sci-hub."))?
            .to_str()
            .or(Err(Error::SciHubParse("Received malformed pdf url from sci-hub.")))
            .map(|pdf_url| String::from(convert_protocol_relative_url_to_absolute(pdf_url, &url)))
    }

    async fn fetch_html_document(&self, url: Url) -> Result<Html, Error> {
        let text = self.client
            .get(url)
            .header(header::ACCEPT, "text/html")
            .send().await?
            .text().await?;
        Ok(Html::parse_document(&text))
    }
}

fn convert_protocol_relative_url_to_absolute(relative_url: &str, absolute_url: &Url) -> String {
    if relative_url.starts_with("//") {
        return format!("{}:{}", absolute_url.scheme(), relative_url);
    } else {
        return String::from(relative_url);
    }
}


#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Paper {
    pub scihub_url: Url,
    pub doi: String,
    pub title: String,
    pub version: String,
    pub download_url: String,
    // pub citation: String,
    pub other_versions: Vec<PaperVersion>
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PaperVersion {
    pub version: String,
    pub scihub_url: String
}