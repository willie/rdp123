//! Native Microsoft sign-in window for RDS AAD authentication.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowDelegate, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, NSURLRequest, NSURL};
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKWebView,
    WKWebViewConfiguration, WKWebsiteDataStore,
};
use tokio::sync::oneshot;
use url::Url;

type SignInReply = oneshot::Sender<Result<String, String>>;

pub struct WebAuthIvars {
    window: RefCell<Option<Retained<NSWindow>>>,
    reply: RefCell<Option<SignInReply>>,
    redirect_uri: String,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123WebAuthController"]
    #[ivars = WebAuthIvars]
    pub struct WebAuthController;

    unsafe impl NSObjectProtocol for WebAuthController {}

    unsafe impl WKNavigationDelegate for WebAuthController {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_navigation(
            &self,
            _web_view: &WKWebView,
            navigation_action: &WKNavigationAction,
            decision_handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let redirected = unsafe { navigation_action.request() }
                .URL()
                .and_then(|url| url.absoluteString())
                .map(|url| url.to_string());
            if let Some(redirected) = redirected {
                if is_oauth_redirect(&redirected, &self.ivars().redirect_uri) {
                    tracing::info!("Microsoft sign-in redirect received");
                    decision_handler.call((WKNavigationActionPolicy::Cancel,));
                    self.finish(Ok(redirected));
                    return;
                }
            }
            decision_handler.call((WKNavigationActionPolicy::Allow,));
        }
    }

    unsafe impl NSWindowDelegate for WebAuthController {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &objc2_foundation::NSNotification) {
            if let Some(reply) = self.ivars().reply.borrow_mut().take() {
                let _ = reply.send(Err("Microsoft sign-in was cancelled".to_string()));
            }
        }
    }
);

impl WebAuthController {
    pub fn new(mtm: MainThreadMarker, redirect_uri: String, reply: SignInReply) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WebAuthIvars {
            window: RefCell::new(None),
            reply: RefCell::new(Some(reply)),
            redirect_uri,
        });
        unsafe { msg_send![super(this), init] }
    }

    pub fn show(&self, mtm: MainThreadMarker, authorization_url: &str) -> Result<(), String> {
        let url = NSURL::URLWithString(&NSString::from_str(authorization_url))
            .ok_or_else(|| "Microsoft returned an invalid sign-in URL".to_string())?;

        let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(780.0, 640.0));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Sign in with Microsoft"));
        window.setMinSize(CGSize::new(560.0, 480.0));
        unsafe { window.setReleasedWhenClosed(false) };
        window.setDelegate(Some(ProtocolObject::from_ref(self)));

        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        // Keep Microsoft's sign-in cookies in memory only, like the tokens.
        unsafe {
            configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm))
        };
        let web_view = unsafe {
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &configuration)
        };
        web_view.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let delegate: &ProtocolObject<dyn WKNavigationDelegate> = ProtocolObject::from_ref(self);
        unsafe { web_view.setNavigationDelegate(Some(delegate)) };
        window.setContentView(Some(&web_view));

        let request = NSURLRequest::requestWithURL(&url);
        unsafe { web_view.loadRequest(&request) };
        window.center();
        window.makeKeyAndOrderFront(None);
        *self.ivars().window.borrow_mut() = Some(window);
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(reply) = self.ivars().reply.borrow_mut().take() {
            let _ = reply.send(Err("Microsoft sign-in was cancelled".to_string()));
        }
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.close();
        }
    }

    fn finish(&self, result: Result<String, String>) {
        if let Some(reply) = self.ivars().reply.borrow_mut().take() {
            let _ = reply.send(result);
        }
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.close();
        }
    }
}

/// Match an OAuth redirect by URL components. Schemes and hosts are
/// case-insensitive; comparing a raw string can miss a valid redirect after
/// WebKit or the identity provider normalizes either one.
fn is_oauth_redirect(candidate: &str, expected: &str) -> bool {
    let (Ok(candidate), Ok(expected)) = (Url::parse(candidate), Url::parse(expected)) else {
        return false;
    };
    candidate.scheme().eq_ignore_ascii_case(expected.scheme())
        && candidate
            .host_str()
            .zip(expected.host_str())
            .is_some_and(|(candidate, expected)| candidate.eq_ignore_ascii_case(expected))
        && candidate.port_or_known_default() == expected.port_or_known_default()
        && candidate.path() == expected.path()
}

#[cfg(test)]
mod tests {
    use super::is_oauth_redirect;

    const REDIRECT: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";

    #[test]
    fn oauth_redirect_matches_components_and_ignores_host_case() {
        assert!(is_oauth_redirect(
            "https://LOGIN.MICROSOFTONLINE.COM/common/oauth2/nativeclient?code=abc&state=xyz",
            REDIRECT
        ));
    }

    #[test]
    fn oauth_redirect_rejects_lookalike_hosts_and_paths() {
        assert!(!is_oauth_redirect(
            "https://login.microsoftonline.com.evil.test/common/oauth2/nativeclient?code=abc",
            REDIRECT
        ));
        assert!(!is_oauth_redirect(
            "https://login.microsoftonline.com/common/oauth2/nativeclient/extra?code=abc",
            REDIRECT
        ));
    }
}
