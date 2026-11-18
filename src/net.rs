//! What every HTTP client in Orb shares: reading a body without trusting
//! the server about its size.

/// Read a response body of at most `max` bytes.
///
/// `Response::bytes` reads whatever the server sends, and a server that
/// sends no `Content-Length` — or lies in it — decides how much memory the
/// client allocates. The body is read in chunks and refused the moment it
/// passes `max`, with only `max` bytes ever held.
pub async fn body_bounded(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return Err(format!("the answer is larger than {}", megabytes(max)));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > max {
            return Err(format!("the answer is larger than {}", megabytes(max)));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn megabytes(bytes: usize) -> String {
    format!("{} MB", bytes / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(body: &'static str, content_length: Option<usize>) -> reqwest::Response {
        let mut builder = http::Response::builder().status(200);
        if let Some(length) = content_length {
            builder = builder.header("content-length", length);
        }
        reqwest::Response::from(builder.body(body).unwrap())
    }

    #[tokio::test]
    async fn a_body_within_the_bound_is_read_whole() {
        let body = body_bounded(answer("ten bytes.", None), 10).await.unwrap();
        assert_eq!(body, b"ten bytes.");
    }

    #[tokio::test]
    async fn a_body_past_the_bound_is_refused_by_its_length_or_by_its_bytes() {
        let declared = body_bounded(answer("ten bytes.", Some(10)), 9).await;
        assert_eq!(declared.unwrap_err(), "the answer is larger than 0 MB");
        let undeclared = body_bounded(answer("ten bytes.", None), 9).await;
        assert!(undeclared.is_err());
    }
}
