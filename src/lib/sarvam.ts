// Sarvam realtime STT language catalog. Codes are the *realtime WebSocket*
// enum (note: Odia is "or-IN" here but "od-IN" on Sarvam's REST endpoints).
// Keep in sync with docs.sarvam.ai — the backend passes the code through
// verbatim.

export interface SarvamLanguage {
  code: string;
  label: string;
}

export const DASHBOARD_URL = "https://dashboard.sarvam.ai";

export const LANGUAGES: SarvamLanguage[] = [
  { code: "auto", label: "Auto-detect (all languages)" },
  { code: "en-IN", label: "English" },
  { code: "hi-IN", label: "Hindi — हिन्दी" },
  { code: "bn-IN", label: "Bengali — বাংলা" },
  { code: "ta-IN", label: "Tamil — தமிழ்" },
  { code: "te-IN", label: "Telugu — తెలుగు" },
  { code: "kn-IN", label: "Kannada — ಕನ್ನಡ" },
  { code: "ml-IN", label: "Malayalam — മലയാളം" },
  { code: "mr-IN", label: "Marathi — मराठी" },
  { code: "gu-IN", label: "Gujarati — ગુજરાતી" },
  { code: "pa-IN", label: "Punjabi — ਪੰਜਾਬੀ" },
  { code: "or-IN", label: "Odia — ଓଡ଼ିଆ" },
  { code: "as-IN", label: "Assamese — অসমীয়া" },
  { code: "ur-IN", label: "Urdu — اردو" },
  { code: "ne-IN", label: "Nepali — नेपाली" },
  { code: "kok-IN", label: "Konkani — कोंकणी" },
  { code: "ks-IN", label: "Kashmiri — کٲشُر" },
  { code: "sd-IN", label: "Sindhi — سنڌي" },
  { code: "sa-IN", label: "Sanskrit — संस्कृतम्" },
  { code: "sat-IN", label: "Santali — ᱥᱟᱱᱛᱟᱲᱤ" },
  { code: "mni-IN", label: "Meitei (Manipuri) — ꯃꯩꯇꯩꯂꯣꯟ" },
  { code: "brx-IN", label: "Bodo — बड़ो" },
  { code: "mai-IN", label: "Maithili — मैथिली" },
  { code: "doi-IN", label: "Dogri — डोगरी" },
];
