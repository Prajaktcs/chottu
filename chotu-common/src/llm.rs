use crate::models::{EmailMetadata, OllamaClassificationResponse};
use rig_core::client::{CompletionClient, Nothing};
use rig_core::completion::Prompt;
use rig_core::message::ToolChoice;
use rig_core::providers::gemini;
use rig_core::providers::ollama;
use rig_core::providers::openrouter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Client error: {0}")]
    Client(String),
    #[error("JSON parsing error: {0}. Raw response was: {1}")]
    JsonParse(serde_json::Error, String),
    #[error("Invalid classification response from model: {0}")]
    InvalidClassification(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct NutritionEstimation {
    pub total_calories: i32,
    pub protein_grams: f64,
    pub carbs_grams: f64,
    pub fats_grams: f64,
    pub dominant_macro: String,
    pub reasoning: String,
    pub omega_3_dha_mg: f64,
    pub cholesterol_mg: f64,
    pub saturated_fat_g: f64,
    pub unsaturated_fat_g: f64,
    pub triglycerides_mg: f64,
    pub iron_mg: f64,
    pub vitamin_b_mg: f64,
    pub vitamin_c_mg: f64,
    pub sugar_g: f64,
    pub fiber_g: f64,
    pub sodium_mg: f64,
    pub potassium_mg: f64,
    pub calcium_mg: f64,
    pub magnesium_mg: f64,
    pub zinc_mg: f64,
    pub vitamin_a_mcg: f64,
    pub vitamin_d_mcg: f64,
    pub vitamin_e_mg: f64,
    pub vitamin_k_mcg: f64,
    pub caffeine_mg: f64,
    pub trans_fat_g: f64,
    /// Closed vocabulary slugs (alcohol, dairy, …). Unknown values are sanitized/dropped during extraction and assignment.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn sanitize_nutrition_tags(mut est: NutritionEstimation) -> NutritionEstimation {
    est.tags = crate::food_tags::sanitize_food_tags(est.tags);
    est
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LedgerExtraction {
    pub amount: f64,
    pub currency: String,
    pub merchant: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActionItemExtraction {
    pub task_description: String,
    /// Optional due date in YYYY-MM-DD if mentioned in the email.
    #[serde(default)]
    pub due_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TravelItineraryExtraction {
    pub destination: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpcomingBillExtraction {
    pub biller: String,
    pub amount: Option<f64>,
    pub due_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PersonalReferenceExtraction {
    pub title: String,
    pub url: Option<String>,
    pub notes: String,
}

/// What a food photo appears to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FoodPhotoKind {
    Barcode,
    Package,
    Plated,
    Unknown,
}

/// Gemini vision analysis of a Telegram food photo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct FoodPhotoAnalysis {
    pub kind: FoodPhotoKind,
    /// Digits read from a barcode when clearly visible.
    #[serde(default)]
    pub barcode: Option<String>,
    /// Human-readable description of the food / product (include portion notes from caption).
    pub description: String,
    /// Estimated nutrition for the portion shown / described.
    pub nutrition: NutritionEstimation,
}

/// High-level Telegram free-text intents (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IntentKind {
    Status,
    Brief,
    Calendar,
    Trends,
    Tasks,
    TaskAdd,
    Sync,
    Food,
    Networth,
    Monthly,
    Budget,
    Memory,
    Plan,
    Help,
    Unknown,
}

/// Meal text + optional resolved log day/time from `/food` or photo captions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct FoodLogContext {
    pub food_description: String,
    /// YYYY-MM-DD when the user named a day; omit for today.
    #[serde(default)]
    pub food_date: Option<String>,
    /// HH:MM local when the user named a meal/time.
    #[serde(default)]
    pub food_time: Option<String>,
}

/// Structured free-text intent classification from local Ollama.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct IntentClassification {
    pub intent: IntentKind,
    /// For TRENDS: lookback days (default 7 if omitted).
    #[serde(default)]
    pub days: Option<i64>,
    /// For CALENDAR: today | tomorrow | week (default today).
    #[serde(default)]
    pub calendar_window: Option<String>,
    /// For TASKS: filter/action args, e.g. "open", "all", "snoozed praj".
    #[serde(default)]
    pub tasks_args: Option<String>,
    /// For TASK_ADD: task title / reminder text.
    #[serde(default)]
    pub task_title: Option<String>,
    /// For TASK_ADD: due phrase, e.g. "tomorrow 3pm", "friday", "2026-08-10".
    #[serde(default)]
    pub due_raw: Option<String>,
    /// For FOOD / TASK_ADD: family member id when mentioned.
    #[serde(default)]
    pub member_id: Option<String>,
    /// For FOOD: meal description text (food only; no date framing).
    #[serde(default)]
    pub food_description: Option<String>,
    /// For FOOD: resolved local civil day as YYYY-MM-DD when the user named a day.
    #[serde(default)]
    pub food_date: Option<String>,
    /// For FOOD: resolved local time as HH:MM when the user named a meal/time.
    #[serde(default)]
    pub food_time: Option<String>,
    /// For MONTHLY: YYYY-MM when mentioned.
    #[serde(default)]
    pub month: Option<String>,
    /// For MEMORY: the recall/search question text.
    #[serde(default)]
    pub memory_query: Option<String>,
    /// For PLAN: true when the user wants a fresh weekly training plan.
    #[serde(default)]
    pub plan_regenerate: Option<bool>,
    /// For UNKNOWN: one short clarifying question for the user.
    #[serde(default)]
    pub clarify_question: Option<String>,
    pub reason: String,
}

/// Dispatch-friendly intent after LLM classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserIntent {
    Status,
    Brief,
    Calendar { window: String },
    Trends { days: Option<i64> },
    Tasks { filter: String },
    TaskAdd {
        member_id: Option<String>,
        title: String,
        due_raw: Option<String>,
    },
    Sync,
    Food {
        member_id: Option<String>,
        description: String,
        /// YYYY-MM-DD when the user named a day; None → log for today.
        date: Option<String>,
        /// HH:MM local when the user named a meal/time; None → now (today) or noon (other day).
        time: Option<String>,
    },
    Networth,
    Monthly { yyyy_mm: Option<String> },
    Budget,
    Memory { query: String },
    /// Weekly training plan; `regenerate` forces `/plan new`.
    Plan { regenerate: bool },
    Help,
    Unknown { clarify_question: String },
}

impl IntentClassification {
    pub fn into_user_intent(self) -> UserIntent {
        match self.intent {
            IntentKind::Status => UserIntent::Status,
            IntentKind::Brief => UserIntent::Brief,
            IntentKind::Calendar => {
                let raw = self
                    .calendar_window
                    .unwrap_or_else(|| "today".to_string())
                    .trim()
                    .to_lowercase();
                let window = if raw.is_empty()
                    || raw == "today"
                    || raw == "day"
                {
                    "today".to_string()
                } else if raw == "tomorrow" || raw == "tmr" || raw == "tmrw" {
                    "tomorrow".to_string()
                } else if raw == "week" || raw == "this week" || raw == "thisweek" {
                    "week".to_string()
                } else {
                    "today".to_string()
                };
                UserIntent::Calendar { window }
            }
            IntentKind::Trends => UserIntent::Trends { days: self.days },
            IntentKind::Tasks => UserIntent::Tasks {
                filter: self
                    .tasks_args
                    .unwrap_or_else(|| "open".to_string())
                    .trim()
                    .to_string(),
            },
            IntentKind::TaskAdd => {
                let title = self.task_title.unwrap_or_default().trim().to_string();
                if title.is_empty() {
                    UserIntent::Unknown {
                        clarify_question: self.clarify_question.unwrap_or_else(|| {
                            "What task should I add? e.g. remind me to call the dentist tomorrow 3pm."
                                .to_string()
                        }),
                    }
                } else {
                    UserIntent::TaskAdd {
                        member_id: self.member_id.filter(|s| !s.trim().is_empty()),
                        title,
                        due_raw: self
                            .due_raw
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    }
                }
            }
            IntentKind::Sync => UserIntent::Sync,
            IntentKind::Food => {
                let description = self
                    .food_description
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if description.is_empty() {
                    UserIntent::Unknown {
                        clarify_question: self.clarify_question.unwrap_or_else(|| {
                            "What did you eat? Include a member id if needed (e.g. praj 2 eggs)."
                                .to_string()
                        }),
                    }
                } else {
                    UserIntent::Food {
                        member_id: self.member_id.filter(|s| !s.trim().is_empty()),
                        description,
                        date: self
                            .food_date
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                        time: self
                            .food_time
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    }
                }
            }
            IntentKind::Networth => UserIntent::Networth,
            IntentKind::Monthly => UserIntent::Monthly {
                yyyy_mm: self.month.filter(|s| !s.trim().is_empty()),
            },
            IntentKind::Budget => UserIntent::Budget,
            IntentKind::Memory => {
                let query = self
                    .memory_query
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if query.is_empty() {
                    UserIntent::Unknown {
                        clarify_question: self.clarify_question.unwrap_or_else(|| {
                            "What should I look up in your journals, digests, notes, or tasks?"
                                .to_string()
                        }),
                    }
                } else {
                    UserIntent::Memory { query }
                }
            }
            IntentKind::Plan => UserIntent::Plan {
                regenerate: self.plan_regenerate.unwrap_or(false),
            },
            IntentKind::Help => UserIntent::Help,
            IntentKind::Unknown => UserIntent::Unknown {
                clarify_question: self.clarify_question.unwrap_or_else(|| {
                    "I didn't catch that — try asking for calendar, brief, status, tasks, remind me, memory, training plan, food log, sync, trends, net worth, monthly spend, or budget."
                        .to_string()
                }),
            },
        }
    }
}

const INTENT_CLASSIFIER_SYSTEM_PROMPT: &str = "\
You classify short personal-assistant messages from Telegram into exactly one intent.\
\
Intents:\
- BRIEF: full morning / day-ahead digest (calendar + tasks + bills + nutrition), e.g. \"morning brief\", \"brief me\", \"day ahead digest\".\
- CALENDAR: agenda / schedule only; set calendar_window to today, tomorrow, or week (default today). e.g. \"what's today\", \"what's on today\", \"tomorrow's schedule\", \"this week\", \"any conflicts\", \"calendar\".\
- STATUS: today finance + health numbers, e.g. \"how's today\", \"status\", \"how am I doing\".\
- TRENDS: multi-day nutrition trends; set days if mentioned (default 7), e.g. \"trends last 14 days\".\
- TASKS: list or manage existing tasks; put filter/action text in tasks_args (e.g. \"open\", \"all\", \"snoozed\", \"complete abc123\").\
- TASK_ADD: create a new task or reminder; put the title in task_title, optional member_id, and optional due_raw (e.g. \"tomorrow 3pm\", \"friday\", \"2026-08-10\"). Examples: \"remind me to call the dentist tomorrow\", \"add task buy milk\", \"todo: pay rent Friday\".\
- MEMORY: recall/search over journals, newsletter digests, personal references, or past tasks; put the question in memory_query (e.g. \"what was that Thai recipe\", \"did I write about the interview\", \"find my note on homelab\").\
- PLAN: weekly training / workout plan; set plan_regenerate true for regenerate / new plan / redo plan. Examples: \"what's today's workout\", \"show my training plan\", \"regenerate plan\", \"beach body plan\".\
- SYNC: pull Google Health / nutrition sync now.\
- FOOD: log a meal; put member_id when named and the meal text in food_description (food only). When the user names when they ate it, resolve to food_date (YYYY-MM-DD) and optional food_time (HH:MM 24h local) using Today's local date from the user message — e.g. \"yesterday\" / \"last night\" / \"Friday\" become concrete dates; meal-of-day windows: breakfast≈08:00, lunch≈12:30 (12:00–13:00), snack(s)≈17:00 (16:00–18:00), dinner/supper≈20:45 (20:00–21:30). Prefer an explicit clock time when the user gave one. Omit food_date/food_time when logging for now/today with no specific meal time.\
- NETWORTH: portfolio / net worth questions.\
- MONTHLY: monthly spending summary; set month as YYYY-MM if given.\
- BUDGET: category spend budgets / how much left this month (e.g. \"budget\", \"how's food budget\", \"am I over on shopping\").\
- HELP: asking what you can do / commands.\
- UNKNOWN: anything else, jokes, unrelated chat — set a short clarify_question.\
\
Rules:\
- Prefer FOOD when the message clearly describes food eaten (even without the word log).\
- Prefer TASK_ADD for \"remind me\", \"add task\", \"todo:\", creating a new reminder/task.\
- Prefer TASKS for listing or managing existing tasks (open/complete/snooze/reassign), not creating.\
- Prefer MEMORY for recall/search questions about notes, journals, digests, recipes, or \"did I write/save...\".\
- Prefer PLAN for training plan / today's workout / regenerate weekly fitness plan asks.\
- Prefer CALENDAR for agenda / schedule / conflicts / what's on today / tomorrow / this week (not the full brief).\
- Prefer BRIEF only for explicit morning brief / full digest phrasing.\
- Prefer BUDGET for category budget progress / overspend questions (not the full monthly ledger summary).\
- Prefer MONTHLY for overall spend summary / category totals for a month.\
- Prefer STATUS for health/finance status without an agenda ask.\
- Never invent a food_description; if intent is FOOD but meal text is missing, use UNKNOWN with a clarify_question.\
- Resolve relative food days/times into food_date/food_time; never leave relative words like \"yesterday\" in food_date — always YYYY-MM-DD. Omit food_date when the meal is for today / unspecified.\
- Never invent memory_query; if intent is MEMORY but the question is missing, use UNKNOWN with a clarify_question.\
- Never invent task_title; if intent is TASK_ADD but title is missing, use UNKNOWN with a clarify_question.\
- member_id must be one of the provided family member ids when set.\
- Keep reason brief.\
";

pub const DEFAULT_EMAIL_CLASSIFIER_SYSTEM_PROMPT: &str = "\
You are an email classification assistant. Your job is to classify the metadata of incoming emails into one of nine categories.\
\
IMPORTANT DISAMBIGUATION RULES (apply these first):\
- LEDGER_STREAM requires evidence of a completed or pending money/points movement (charged, paid, transferred, redeemed, order placed with amount). Marketing that says \"Get X points\", \"% off\", or \"deals\" is NOT LEDGER_STREAM — use TRASH.\
- \"Your bill has been paid\" / autopay success notices with no purchase details are ARCHIVE (confirmation), not LEDGER_STREAM or FINANCIAL_BILL.\
- FINANCIAL_BILL is for amounts still due / upcoming payments, not past-tense \"paid\" confirmations.\
- NEWSLETTER is for subscribed digests you read (finance, markets, tech, word-of-the-day, quizzes, home tips). Do not put those in ARCHIVE or TRASH.\
- TRASH is for sales pitches, discount blasts, and cold promo — not for subscribed educational/market digests.\
- House tips / lifestyle content from publishers (e.g. House Outlook) is NEWSLETTER, never LEDGER_STREAM.\
- Standalone local parking (SpotHero/ParkWhiz) is LEDGER_STREAM if paid; airport/trip parking is TRAVEL_ITINERARY.\
- Broker/bank *market* alerts (\"traded above high volume\", price alerts, smart alerts) are NEWSLETTER or ARCHIVE — never LEDGER_STREAM. Share volume is not a dollar amount.\
- Low-balance / available-balance threshold alerts are ARCHIVE, not LEDGER_STREAM (they are not debits/credits).\
- Canceled/cancelled order notices are ARCHIVE, not LEDGER_STREAM.\
- Credit-card \"authorization\" / foreign-transaction notices are ARCHIVE (holds), not LEDGER_STREAM unless a settled purchase receipt.\
- Generic \"Direct Deposit Greater Than $X\" threshold alerts are ARCHIVE, not LEDGER_STREAM.\
- Forum/community digests (Reddit, etc.) are NEWSLETTER or TRASH — never LEDGER_STREAM even if the post mentions money.\
- Loan spam / pre-approval pitches (\"personalized loan\", \"superfast approval\") are TRASH.\
- ACTION_ITEM is only for real personal/work commitments you must do (approve, reply to a person, submit something with a deadline). Social notifications, job-alert blasts, \"rate your purchase\", survey/test invitations, and charity signup nudges are NOT ACTION_ITEM — use TRASH or ARCHIVE.\
- TRAVEL_ITINERARY requires a real booking (confirmation code, flight/hotel/parking dates). Travel deal emails, flight credits, cruise marketing, and standalone local parking (SpotHero) are NOT travel.\
\
1. LEDGER_STREAM:\
   - Must be used for transactional emails with actual financial events: purchase receipts, order invoices/confirmations, refunds, payment requests, money transfers, dividend payouts, bank debit/credit alerts, or completed points redemptions.\
   - Examples of LEDGER_STREAM: Amazon order confirmations, Google Play receipts, Lyft ride receipts, SpotHero/ParkWhiz local parking purchases, Interac e-Transfers, Wealthsimple dividend payouts, HDFC transaction alerts, \"You redeemed 30,000 PC Optimum points\".\
   - Do NOT use for: \"Get N points\" offers, coupon/promo emails, tips/content newsletters, \"bill has been paid\" confirmations, volume/price alerts, balance-threshold alerts, canceled orders, or loan marketing.\
\
2. ARCHIVE:\
   - Must be used for important non-marketing notifications, personal correspondence, account notices, security alerts, and system notices — including past-tense bill/autopay paid confirmations.\
   - Examples of ARCHIVE: Security sign-in alerts, password changed alerts, \"Your bill has been paid\", monthly statement-available notices (without PDF attachment workflow), Jira/GitHub build status.\
\
3. TRASH:\
   - Must be used for spam, marketing, ads, promotions, sales pitches, and low-value blasts that are not subscribed digests you keep.\
   - Examples of TRASH: MealPal lunch reminders, \"50% off everything\", \"Get 30,000 PC Optimum points\", Kayak/airline deal blasts, insurance quote spam, fitness membership promo (\"Still thinking about…?\").\
\
4. ACTION_ITEM:\
   - Must be used only for emails that create a real personal or work commitment you need to complete.\
   - Examples of ACTION_ITEM: \"Action required: approve design\", \"please review this PR\", \"Task assigned to you\", a person asking you to send a document by Friday.\
   - Do NOT use for: LinkedIn \"you have a new message\" / connection / recommendation notifications, LinkedIn Job Alerts or \"apply now\" blasts, Amazon/marketplace \"rate your transaction\", UserTesting or survey invites, charity/fundraising nudges (\"It starts next week\"), community digests, or generic \"view this\" CTAs. Those are TRASH (promo/engagement) or ARCHIVE (passive notification).\
\
5. TRAVEL_ITINERARY:\
   - Must be used for trips and trip logistics: flights, hotels, buses/trains, car rentals, airport transfers, and parking only when clearly part of a trip (airport/hotel parking for travel dates).\
   - Examples of TRAVEL_ITINERARY: Flight booking confirmation, Expedia itinerary, Airbnb reservation, airport parking for YYZ Jun 20–22.\
   - Do NOT use for standalone local parking passes (SpotHero/ParkWhiz) with no trip context — those are LEDGER_STREAM (if paid) or ARCHIVE (pass-only).\
   - Do NOT use for travel marketing: flight credits, \"deals from $X\", cruise promotions, highway/toll offers (407 ETR), or \"edit your trip\" nudges without a confirmed itinerary.\
\
6. FINANCIAL_BILL:\
   - Must be used for future bills, invoices to be paid, payment reminders, or upcoming automatic payments still due.\
   - Examples of FINANCIAL_BILL: Rogers bill due June 15, electric utility payment reminder, upcoming rent invoice.\
   - Do NOT use for \"Your bill has been paid\" (that is ARCHIVE).\
\
7. STATEMENT_DOCUMENT:\
   - Must be used for monthly statements, pay stubs, tax documents, or payslips that typically contain PDF attachments.\
   - Examples of STATEMENT_DOCUMENT: Monthly banking statement, pay stub notice, tax receipt/slip, credit card statement with attachment.\
\
8. NEWSLETTER:\
   - Must be used for subscribed digests you want to keep up with: tech/finance/markets blogs, Substack, educational dailies (word of the day, quiz), and lifestyle/home tip publishers.\
   - Examples of NEWSLETTER: Rust Weekly, Robinhood Snacks, Finimize Daily, Mint/Livemint market briefs, Word Daily, Quiz Daily, House Outlook tip emails, Substack issue digests.\
   - Do NOT classify these as ARCHIVE or TRASH.\
\
9. PERSONAL_REFERENCE:\
   - Must be used for personal notes, bookmarked articles, recipes, instructions, reference guides, or self-addressed emails with information you want to save.\
   - Examples of PERSONAL_REFERENCE: Recipe to try, homelab setup commands, link/article to read later.\
\
FEW-SHOT EXAMPLES:\
\
Sender: \"Amazon.in\" <auto-confirm@amazon.in>\
Subject: Your Amazon.in order #405-1405094-1960341 of 1 item\
Classification: LEDGER_STREAM (Reason: Amazon order receipt is a financial transaction)\
\
Sender: \"Simplii Financial\" <catch@payments.interac.ca>\
Subject: Interac e-Transfer: The request for $200.00 transfer to PRAJAKT\
Classification: LEDGER_STREAM (Reason: Interac e-Transfer request involves money transaction)\
\
Sender: \"PC Express\" <noreply@e.pcexpress.ca>\
Subject: Get 30,000 PC Optimum points\
Classification: TRASH (Reason: Points marketing offer, not a completed redemption or purchase)\
\
Sender: \"Wealthsimple\" <notifications@o.wealthsimple.com>\
Subject: Your bill has been paid\
Classification: ARCHIVE (Reason: Autopay/bill-paid confirmation, not a ledger purchase)\
\
Sender: \"Robinhood\" <notifications@robinhood.com>\
Subject: Your account statement is available\
Classification: ARCHIVE (Reason: Official monthly financial account statement notice)\
\
Sender: \"amazon.in\" <account-update@amazon.in>\
Subject: amazon.in: Sign-in\
Classification: ARCHIVE (Reason: Security sign-in alert)\
\
Sender: \"David Mollitor (Jira)\" <jira@apache.org>\
Subject: [jira] [Created] (KAFKA-9443) Producer Can Fail with NPE\
Classification: ARCHIVE (Reason: Developer task tracking notification, not marketing)\
\
Sender: \"Udemy Instructor\" <no-reply@e.udemymail.com>\
Subject: Going LIVE today.\
Classification: TRASH (Reason: Promotional marketing)\
\
Sender: \"Github Tasks\" <noreply@github.com>\
Subject: alexexample, please review this PR\
Classification: ACTION_ITEM (Reason: PR review request is an action item for the user)\
\
Sender: \"Air Canada\" <flightconfirmation@aircanada.ca>\
Subject: Your booking confirmation for Montreal (YUL) to Toronto (YYZ)\
Classification: TRAVEL_ITINERARY (Reason: Flight booking details for upcoming trip)\
\
Sender: \"SpotHero Support\" <support@spothero.com>\
Subject: SpotHero Parking Confirmation - Check Your Parking Pass #129593055\
Classification: LEDGER_STREAM (Reason: Standalone local parking purchase/pass with no trip context; not a travel itinerary)\
\
Sender: \"YYZ Airport Parking\" <noreply@torontopearson.com>\
Subject: Your airport parking reservation for YYZ — Jun 20–22\
Classification: TRAVEL_ITINERARY (Reason: Airport parking clearly tied to travel dates)\
\
Sender: \"Rogers Wireless\" <rogers-bill@rogers.com>\
Subject: Your Rogers bill is ready to view - Due Date: June 15, 2026\
Classification: FINANCIAL_BILL (Reason: Future phone bill notification with a due date)\
\
Sender: \"ADP Payslip\" <noreply@adp.com>\
Subject: Your pay statement is now available\
Classification: STATEMENT_DOCUMENT (Reason: Pay statement/payslip containing statement attachments)\
\
Sender: \"This Week in Rust\" <newsletter@thisweekinrust.org>\
Subject: This Week in Rust #650\
Classification: NEWSLETTER (Reason: Tech/programming community newsletter subscription)\
\
Sender: Finimize Daily <hello@finimize.com>\
Subject: A tale of two tech companies\
Classification: NEWSLETTER (Reason: Subscribed finance/markets daily digest)\
\
Sender: \"Word Daily\" <hello@worddaily.com>\
Subject: Word of the Day: Psephology\
Classification: NEWSLETTER (Reason: Subscribed educational word-of-the-day digest)\
\
Sender: \"Quiz Daily\" <mail@quizdaily.com>\
Subject: After water, what is the most consumed beverage in the world?\
Classification: NEWSLETTER (Reason: Subscribed daily quiz digest, not promo spam)\
\
Sender: \"House Outlook\" <hello@houseoutlook.com>\
Subject: You're Cleaning Baseboards the Hard Way\
Classification: NEWSLETTER (Reason: Home tips publisher content, not a paid service receipt)\
\
Sender: IQalerts@questrade.com\
Subject: Alert: DDOG traded above high volume 6,250,982\
Classification: NEWSLETTER (Reason: Broker market/volume alert; share volume is not a ledger transaction)\
\
Sender: PNC Alerts <pncalerts@pnc.com>\
Subject: Your Checking Account Available Balance Is Less Than $100.00\
Classification: ARCHIVE (Reason: Low-balance threshold alert, not a debit or credit)\
\
Sender: Coinbase <contact@coinbase.com>\
Subject: We've canceled your order for 0.52 ETH\
Classification: ARCHIVE (Reason: Canceled order notice; no completed money movement)\
\
Sender: FastApproval <aparna@maldiver.info>\
Subject: SuperFast Approval for Your Personalized Loan For Rs 1,50,000\
Classification: TRASH (Reason: Unsolicited loan spam / pre-approval pitch)\
\
Sender: Reddit <noreply@redditmail.com>\
Subject: \"Metal card came in\"\
Classification: NEWSLETTER (Reason: Forum digest/post notification, not a financial transaction)\
\
Sender: PNC Alerts <pncalerts@pnc.com>\
Subject: Authorization on your credit card outside of Canada\
Classification: ARCHIVE (Reason: Card authorization/hold notice, not a settled ledger purchase)\
\
Sender: LinkedIn <notifications-noreply@linkedin.com>\
Subject: You have 1 new message\
Classification: ARCHIVE (Reason: Passive social notification; not a real commitment)\
\
Sender: LinkedIn Job Alerts <jobalerts-noreply@linkedin.com>\
Subject: Data Analyst, Principal at Dayforce: up to CA$172K/year\
Classification: TRASH (Reason: Automated job-alert marketing blast, not a task you committed to)\
\
Sender: LinkedIn <jobs-noreply@linkedin.com>\
Subject: Prajakt, apply now to 'Staff Software Engineer'\
Classification: TRASH (Reason: Job recommendation CTA, not an actionable personal commitment)\
\
Sender: Amazon Marketplace <marketplace-messages@amazon.ca>\
Subject: Prajakt Chandrashekhar, will you rate your transaction at Amazon.ca?\
Classification: TRASH (Reason: Marketplace rating nudge / engagement promo)\
\
Sender: UserTesting <noreply@usertesting.com>\
Subject: New test opportunity for you!\
Classification: TRASH (Reason: Paid-test / survey recruitment blast)\
\
Sender: \"Ashley (Great Cycle Challenge)\" <hello@greatcyclechallenge.ca>\
Subject: It starts next week...\
Classification: TRASH (Reason: Charity signup / fundraising nudge)\
\
Sender: Expedia.ca <email@expediamail.com>\
Subject: Flights + C$300 credit\
Classification: TRASH (Reason: Travel deal / credit marketing, not a booking confirmation)\
\
Sender: \"Alex Example\" <alex@example.com>\
Subject: Reference: SSH setup commands for homelab\
Classification: PERSONAL_REFERENCE (Reason: Personal note containing reference instructions)\
\
Submit a structured classification with a brief reason explaining why that category was chosen.\
";

#[derive(Debug, Clone)]
pub struct ChotuLlm {
    client: ollama::Client,
    model: String,
    prompt_path: Option<String>,
}

impl ChotuLlm {
    /// Creates a new `ChotuLlm` connector.
    /// `host` should be a URL scheme + host, e.g. "http://localhost"
    pub fn new(host: &str, port: u16, model: &str) -> Self {
        let base_url = format!("{}:{}", host, port);
        let client = ollama::Client::builder()
            .api_key(Nothing)
            .base_url(&base_url)
            .build()
            .unwrap();
        Self {
            client,
            model: model.to_string(),
            prompt_path: None,
        }
    }

    /// Creates a new `ChotuLlm` connector using default settings (http://localhost:11434).
    pub fn new_default(model: &str) -> Self {
        Self {
            client: ollama::Client::new(Nothing).unwrap(),
            model: model.to_string(),
            prompt_path: None,
        }
    }

    /// Set an optional path to a text file containing a custom system prompt.
    pub fn with_prompt_path(mut self, path: Option<String>) -> Self {
        self.prompt_path = path;
        self
    }

    /// General text generation using local Ollama.
    pub async fn generate_prompt(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, LlmError> {
        let agent = self
            .client
            .agent(&self.model)
            .preamble(system_prompt)
            .build();

        let response = agent
            .prompt(user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))?;

        Ok(response)
    }

    /// Faster generation for grounded tasks (memory RAG): no thinking, capped output.
    pub async fn generate_prompt_fast(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, LlmError> {
        let agent = self
            .client
            .agent(&self.model)
            .preamble(system_prompt)
            .temperature(0.2)
            .max_tokens(400)
            .additional_params(serde_json::json!({
                "think": false,
                "num_predict": 400,
            }))
            .build();

        let response = agent
            .prompt(user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))?;

        Ok(response)
    }

    /// Public structured extraction against the configured Ollama model.
    pub async fn extract_typed<T>(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<T, LlmError>
    where
        T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    {
        self.extract_structured(system_prompt, user_prompt).await
    }

    /// Structured extraction via Rig's tool-calling extractor.
    /// Retries help smaller Ollama models that occasionally skip the `submit` tool.
    async fn extract_structured<T>(
        &self,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<T, LlmError>
    where
        T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    {
        let extractor = self
            .client
            .extractor::<T>(&self.model)
            .preamble(system_prompt)
            .retries(2)
            .build();

        extractor
            .extract(user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))
    }

    fn format_email_user_prompt(email: &EmailMetadata) -> String {
        format!(
            "Sender: {}\nSubject: {}\nBody Preview: {}\n",
            email.sender,
            email.subject,
            email
                .body_preview
                .as_deref()
                .unwrap_or("[No body preview provided]")
        )
    }

    /// Classifies an email using the local Ollama LLM.
    pub async fn classify_email(
        &self,
        email: &EmailMetadata,
        unactionable_examples: &[String],
    ) -> Result<OllamaClassificationResponse, LlmError> {
        let system_prompt_owned;
        let system_prompt = if let Some(ref path) = self.prompt_path {
            match tokio::fs::read_to_string(path).await {
                Ok(content) => {
                    system_prompt_owned = content;
                    &system_prompt_owned
                }
                Err(e) => {
                    eprintln!("Warning: Failed to read email classifier system prompt from {:?}: {:?}. Falling back to default system prompt.", path, e);
                    DEFAULT_EMAIL_CLASSIFIER_SYSTEM_PROMPT
                }
            }
        } else {
            DEFAULT_EMAIL_CLASSIFIER_SYSTEM_PROMPT
        };

        let mut user_prompt = Self::format_email_user_prompt(email);

        if !unactionable_examples.is_empty() {
            user_prompt.push_str("\nCRITICAL - USER FEEDBACK (UNACTIONABLE/NOT USEFUL EMAILS):\n");
            user_prompt.push_str("The user has marked the following email descriptions as NOT useful or UNACTIONABLE. Do NOT classify the current email as ACTION_ITEM if it is semantically similar to any of these:\n");
            for (idx, example) in unactionable_examples.iter().enumerate() {
                user_prompt.push_str(&format!("{}. {}\n", idx + 1, example));
            }
            user_prompt.push_str("\nIf the current email matches any of these, classify it as TRASH or ARCHIVE instead of ACTION_ITEM.\n");
        }

        self.extract_structured(system_prompt, &user_prompt).await
    }

    /// Extracts transaction details from a transactional email using local Ollama.
    pub async fn extract_ledger_transaction(
        &self,
        email: &EmailMetadata,
    ) -> Result<LedgerExtraction, LlmError> {
        let system_prompt = "\
You are a financial transaction extraction assistant. Analyze the email headers and body preview \
and extract the transaction amount (0.0 if unknown or if this is not a real money movement), \
currency code (default USD), merchant name, and category (Shopping, Food, Entertainment, \
Utilities, Investment, Subscription, Travel, or Other).\
\
CRITICAL amount rules:\
- Extract only the charged/paid/transferred/refunded cash amount for a completed or pending payment.\
- Never use share volume, trade volume, account numbers, available balances, credit limits, or loan offers as the amount.\
- Market alerts, balance-threshold alerts, canceled orders, and marketing pitches → amount 0.0.";

        self.extract_structured(system_prompt, &Self::format_email_user_prompt(email))
            .await
    }

    /// Extracts action item details from an email using local Ollama.
    pub async fn extract_action_item(
        &self,
        email: &EmailMetadata,
    ) -> Result<ActionItemExtraction, LlmError> {
        let system_prompt = "\
You are an action item extraction assistant. Extract the main task or action request from the email, \
plus an optional due date in YYYY-MM-DD form when one is mentioned. \
Only extract a real commitment. If the email is a social notification, job alert, rating request, \
survey invite, or marketing CTA, still return a short task_description but leave due_date null.";

        self.extract_structured(system_prompt, &Self::format_email_user_prompt(email))
            .await
    }

    /// Extracts travel itinerary details from an email using local Ollama.
    pub async fn extract_travel_itinerary(
        &self,
        email: &EmailMetadata,
    ) -> Result<TravelItineraryExtraction, LlmError> {
        let system_prompt = "\
You are a travel itinerary extraction assistant. Extract destination (city/airport/facility; \
default Unknown only if truly unavailable), optional start/end dates in YYYY-MM-DD, and a concise \
summary of flight numbers, confirmation codes, hotels, parking locations/pass numbers, or other \
booking details. For parking tied to a trip, use the airport/city/garage name as destination.";

        self.extract_structured(system_prompt, &Self::format_email_user_prompt(email))
            .await
    }

    /// Extracts upcoming bill details from an email using local Ollama.
    pub async fn extract_upcoming_bill(
        &self,
        email: &EmailMetadata,
    ) -> Result<UpcomingBillExtraction, LlmError> {
        let system_prompt = "\
You are a bill extraction assistant. Extract the biller name, optional amount, and optional due date \
in YYYY-MM-DD form from the email metadata and body.";

        self.extract_structured(system_prompt, &Self::format_email_user_prompt(email))
            .await
    }

    /// Classifies free-text Telegram messages into a structured user intent.
    pub async fn classify_intent(
        &self,
        text: &str,
        family_member_ids: &[String],
    ) -> Result<IntentClassification, LlmError> {
        let members = if family_member_ids.is_empty() {
            "(none configured)".to_string()
        } else {
            family_member_ids.join(", ")
        };
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let user_prompt = format!(
            "Today's local date: {}\nFamily member ids: {}\nUser message: {}\n",
            today,
            members,
            text.trim()
        );
        self.extract_structured(INTENT_CLASSIFIER_SYSTEM_PROMPT, &user_prompt)
            .await
    }

    /// Resolve meal text + optional log day/time from a food description (for `/food`).
    pub async fn extract_food_log_context(
        &self,
        text: &str,
    ) -> Result<FoodLogContext, LlmError> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let system_prompt = "\
You extract food-log fields from a short user message.\
\
Return:\
- food_description: the food/meal text only (no date/time framing words).\
- food_date: YYYY-MM-DD when the user named a day (resolve relative phrases using Today's local date); omit or null when logging for today / unspecified.\
- food_time: HH:MM 24-hour local when the user named a clock time or meal-of-day (breakfast≈08:00, lunch≈12:30 for 12:00–13:00, snack(s)≈17:00 for 16:00–18:00, dinner/supper≈20:45 for 20:00–21:30); prefer an explicit clock time when given; omit when unspecified.\
\
Never invent food that was not mentioned. Never leave relative words in food_date — always YYYY-MM-DD.\
";
        let user_prompt = format!(
            "Today's local date: {}\nUser message: {}\n",
            today,
            text.trim()
        );
        self.extract_structured(system_prompt, &user_prompt).await
    }

    /// Extracts personal reference details from an email using local Ollama.
    pub async fn extract_personal_reference(
        &self,
        email: &EmailMetadata,
    ) -> Result<PersonalReferenceExtraction, LlmError> {
        let system_prompt = "\
You are a personal reference extraction assistant. Extract a clean title, optional URL, and concise \
notes from the email metadata and body.";

        self.extract_structured(system_prompt, &Self::format_email_user_prompt(email))
            .await
    }
}

// -------------------------------------------------------------
// OpenRouter Client
// -------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct OpenRouterClient {
    client: openrouter::Client,
}

impl OpenRouterClient {
    pub fn new(api_key: impl AsRef<str>) -> Result<Self, LlmError> {
        let client = openrouter::Client::new(api_key.as_ref())
            .map_err(|e| LlmError::Client(format!("Failed to init OpenRouter client: {e}")))?;
        Ok(Self { client })
    }

    /// Build from `OPENROUTER_API_KEY`.
    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| {
            LlmError::Client("OPENROUTER_API_KEY environment variable is not set".into())
        })?;
        if api_key.trim().is_empty() {
            return Err(LlmError::Client(
                "OPENROUTER_API_KEY environment variable is empty".into(),
            ));
        }
        Self::new(api_key)
    }

    /// Plain-text generation via OpenRouter for the given model slug.
    pub async fn generate_prompt(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, LlmError> {
        let agent = self
            .client
            .agent(model)
            .preamble(system_prompt)
            .build();

        agent
            .prompt(user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))
    }

    /// Structured extraction via Rig's tool-calling extractor.
    ///
    /// Qwen thinking models on Alibaba reject `tool_choice: required`, so those
    /// use `auto`. Other OpenRouter models keep Rig's `required` default.
    pub async fn generate_structured<T>(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<T, LlmError>
    where
        T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    {
        let tool_choice = if model.starts_with("qwen/") {
            ToolChoice::Auto
        } else {
            ToolChoice::Required
        };

        let extractor = self
            .client
            .extractor::<T>(model)
            .preamble(system_prompt)
            .tool_choice(tool_choice)
            .retries(2)
            .build();

        extractor
            .extract(user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))
    }
}

// -------------------------------------------------------------
// Gemini Client
// -------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GeminiClient {
    client: gemini::Client,
    api_key: String,
}

impl GeminiClient {
    pub fn new(api_key: String) -> Self {
        Self {
            client: gemini::Client::new(&api_key).unwrap(),
            api_key,
        }
    }

    /// Allows custom base url for testing / mocking
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        let client = gemini::Client::builder()
            .api_key(&api_key)
            .base_url(&base_url)
            .build()
            .unwrap();
        Self { client, api_key }
    }

    pub async fn extract_from_document(
        &self,
        doc_path: &std::path::Path,
    ) -> Result<crate::models::DroppedDocumentExtraction, LlmError> {
        // Read file bytes
        let file_bytes = tokio::fs::read(doc_path)
            .await
            .map_err(|e| LlmError::Client(format!("Failed to read document file: {:?}", e)))?;

        // Base64 encode the bytes
        use base64::prelude::*;
        let base64_data = BASE64_STANDARD.encode(&file_bytes);

        // Determine mime type
        let ext = doc_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let mime_type = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "pdf" => "application/pdf",
            _ => "image/png", // fallback
        };

        // Construct Gemini URL
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.6-flash:generateContent?key={}",
            self.api_key
        );

        // Construct prompt
        let prompt_text = "\
You are a financial and document parser assistant. Analyze this document (image or PDF) to determine its document type:
- If it is a financial receipt or invoice, set document_type to \"RECEIPT\", extract the transaction details, and populate receipt_transaction.
- If it is a stock portfolio statement or holding status screenshot/document, set document_type to \"PORTFOLIO\", extract all stock holdings, and populate portfolio_holdings.

You MUST return ONLY a JSON object matching this schema:
{
  \"document_type\": \"RECEIPT\" | \"PORTFOLIO\",
  \"receipt_transaction\": null | {
    \"amount\": number (total amount, e.g. 42.50. Set to 0.0 if not found),
    \"currency\": \"USD\" | \"CAD\" | \"EUR\" | \"GBP\" | \"INR\" | string (currency code, default to \"USD\"),
    \"merchant\": \"Name of the merchant/store/service\",
    \"category\": \"Shopping\" | \"Food\" | \"Entertainment\" | \"Utilities\" | \"Investment\" | \"Subscription\" | \"Travel\" | \"Other\"
  },
  \"portfolio_holdings\": null | [
    {
      \"ticker\": \"Stock ticker symbol, e.g. AAPL, MSFT, TSLA\",
      \"shares_owned\": number (number of shares),
      \"average_cost\": number (average cost per share),
      \"average_cost_currency\": \"USD\" | \"CAD\" | \"EUR\" | \"GBP\" | \"INR\" | string | null (currency of average_cost as shown on the statement; null if unclear)
    }
  ]
}
Do not include any explanation or markdown formatting outside the JSON block.";

        let payload = serde_json::json!({
            "contents": [
                {
                    "parts": [
                        { "text": prompt_text },
                        {
                            "inlineData": {
                                "mimeType": mime_type,
                                "data": base64_data
                            }
                        }
                    ]
                }
            ],
            "generationConfig": {
                "responseMimeType": "application/json"
            }
        });

        let http_client = reqwest::Client::new();
        let res = http_client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| LlmError::Client(format!("Gemini request failed: {:?}", e)))?;

        let res_json: serde_json::Value = res
            .json()
            .await
            .map_err(|e| LlmError::Client(format!("Failed to parse Gemini response JSON: {:?}", e)))?;

        // Extract text from the response
        let text_response = res_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .ok_or_else(|| {
                LlmError::Client(format!(
                    "Unexpected response structure from Gemini API: {:?}",
                    res_json
                ))
            })?;

        let cleaned = clean_json_response(text_response);
        let parsed: crate::models::DroppedDocumentExtraction = serde_json::from_str(&cleaned)
            .map_err(|e| LlmError::JsonParse(e, text_response.to_string()))?;

        Ok(parsed)
    }

    /// Analyze a food photo (barcode, package, or plated meal) with optional caption.
    pub async fn approximate_nutrition_from_image(
        &self,
        image_bytes: &[u8],
        mime_type: &str,
        caption: &str,
    ) -> Result<FoodPhotoAnalysis, LlmError> {
        use base64::prelude::*;
        let base64_data = BASE64_STANDARD.encode(image_bytes);

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.6-flash:generateContent?key={}",
            self.api_key
        );

        let caption_line = if caption.trim().is_empty() {
            "(no caption)".to_string()
        } else {
            caption.trim().to_string()
        };

        let prompt_text = format!(
            "You are a professional nutritionist analyzing a Telegram food photo.\n\
User caption (may include family member id and portion notes): {caption_line}\n\n\
Decide what the image shows:\n\
- BARCODE: a product barcode is clearly readable — set barcode to the digits only.\n\
- PACKAGE: packaged food / nutrition label / product packaging without a readable barcode.\n\
- PLATED: prepared or plated food.\n\
- UNKNOWN: not food-related.\n\n\
Write description as a concise meal/product log line (include caption portion notes when relevant).\n\
Estimate nutrition for the portion the user likely ate (use caption for half/shared/portion hints; otherwise one typical serving).\n\
Include calories, macros (g), and key micros matching this JSON schema exactly.\n\
Return ONLY JSON:\n\
{{\n\
  \"kind\": \"BARCODE\" | \"PACKAGE\" | \"PLATED\" | \"UNKNOWN\",\n\
  \"barcode\": null | \"digits\",\n\
  \"description\": \"string\",\n\
  \"nutrition\": {{\n\
    \"total_calories\": int,\n\
    \"protein_grams\": number,\n\
    \"carbs_grams\": number,\n\
    \"fats_grams\": number,\n\
    \"dominant_macro\": \"protein\" | \"carbs\" | \"fat\" | string,\n\
    \"reasoning\": \"brief\",\n\
    \"omega_3_dha_mg\": number,\n\
    \"cholesterol_mg\": number,\n\
    \"saturated_fat_g\": number,\n\
    \"unsaturated_fat_g\": number,\n\
    \"triglycerides_mg\": number,\n\
    \"iron_mg\": number,\n\
    \"vitamin_b_mg\": number,\n\
    \"vitamin_c_mg\": number,\n\
    \"sugar_g\": number,\n\
    \"fiber_g\": number,\n\
    \"sodium_mg\": number,\n\
    \"potassium_mg\": number,\n\
    \"calcium_mg\": number,\n\
    \"magnesium_mg\": number,\n\
    \"zinc_mg\": number,\n\
    \"vitamin_a_mcg\": number,\n\
    \"vitamin_d_mcg\": number,\n\
    \"vitamin_e_mg\": number,\n\
    \"vitamin_k_mcg\": number,\n\
    \"caffeine_mg\": number,\n\
    \"trans_fat_g\": number,\n\
    \"tags\": [\"alcohol\" | \"added_sugar\" | \"dairy\" | \"gluten\" | \"red_meat\" | \"processed_meat\" | \"fried\" | \"spicy\" | \"nightshades\" | \"caffeine\" | \"shellfish\" | \"eggs\" | \"soy\" | \"citrus\"]\n\
  }}\n\
}}\n\
Pick zero or more tags from that closed list only. Do not invent tags.\n\
For UNKNOWN, still return nutrition zeros and explain in reasoning."
        );

        let payload = serde_json::json!({
            "contents": [{
                "parts": [
                    { "text": prompt_text },
                    {
                        "inlineData": {
                            "mimeType": mime_type,
                            "data": base64_data
                        }
                    }
                ]
            }],
            "generationConfig": {
                "responseMimeType": "application/json"
            }
        });

        let http_client = reqwest::Client::new();
        let res = http_client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| LlmError::Client(format!("Gemini food-photo request failed: {:?}", e)))?;

        let status = res.status();
        let res_json: serde_json::Value = res.json().await.map_err(|e| {
            LlmError::Client(format!("Failed to parse Gemini food-photo JSON: {:?}", e))
        })?;

        if !status.is_success() {
            return Err(LlmError::Client(format!(
                "Gemini food-photo returned {}: {}",
                status.as_u16(),
                res_json
            )));
        }

        let text_response = res_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .ok_or_else(|| {
                LlmError::Client(format!(
                    "Unexpected Gemini food-photo response: {:?}",
                    res_json
                ))
            })?;

        let cleaned = clean_json_response(text_response);
        let mut parsed: FoodPhotoAnalysis = serde_json::from_str(&cleaned)
            .map_err(|e| LlmError::JsonParse(e, text_response.to_string()))?;

        // Normalize barcode to digits only when present.
        if let Some(ref code) = parsed.barcode {
            let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
            parsed.barcode = if digits.is_empty() {
                None
            } else {
                Some(digits)
            };
        }
        parsed.nutrition = sanitize_nutrition_tags(parsed.nutrition);

        Ok(parsed)
    }

    pub async fn approximate_nutrition(
        &self,
        food_description: &str,
    ) -> Result<NutritionEstimation, LlmError> {
        let system_prompt = format!(
            "You are a professional nutritionist. Analyze the food description provided by the user, estimate its calories, macronutrients (protein, carbs, fat in grams), and key micronutrients: \
             omega-3 DHA (mg), cholesterol (mg), saturated fat (g), unsaturated fat (g), triglycerides (mg), iron (mg), vitamin B's (mg), vitamin C (mg), \
             sugar (g), fiber (g), sodium (mg), potassium (mg), calcium (mg), magnesium (mg), zinc (mg), vitamin A (mcg), vitamin D (mcg), vitamin E (mg), vitamin K (mcg), caffeine (mg), and trans fat (g). \
             Identify the dominant macronutrient and provide brief reasoning. {}",
            crate::food_tags::food_tag_classifier_instruction()
        );
        let user_prompt = format!("Food description: {}", food_description);

        let extractor = self
            .client
            .extractor::<NutritionEstimation>("gemini-3.6-flash")
            .preamble(&system_prompt)
            .build();

        let response = extractor
            .extract(&user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))?;

        Ok(sanitize_nutrition_tags(response))
    }

    /// Estimates typical Omega-3 DHA and Triglycerides based on total daily fats/calories
    pub async fn estimate_missing_sync_nutrients(
        &self,
        calories: f64,
        protein: f64,
        carbs: f64,
        fat: f64,
    ) -> Result<MissingSyncNutrition, LlmError> {
        let system_prompt = "You are a professional nutritionist. Based on a daily summary (calories, protein, carbs, and fat in grams), estimate the typical standard daily intake of Omega-3 DHA (mg) and Triglycerides (mg) that would correspond to such a diet (assuming normal standard healthy meals).";
        let user_prompt = format!(
            "Daily intake: {} kcal, {}g protein, {}g carbs, {}g fat",
            calories, protein, carbs, fat
        );

        let extractor = self
            .client
            .extractor::<MissingSyncNutrition>("gemini-3.6-flash")
            .preamble(system_prompt)
            .build();

        let response = extractor
            .extract(&user_prompt)
            .await
            .map_err(|e| LlmError::Client(e.to_string()))?;

        Ok(response)
    }

    /// Sends a free-form prompt to Gemini and returns the plain text response.
    /// Useful for composing narrative text like morning briefs and weekly prep notes.
    pub async fn ask(&self, prompt: &str) -> Result<String, LlmError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.6-flash:generateContent?key={}",
            self.api_key
        );

        let body = serde_json::json!({
            "contents": [{
                "parts": [{"text": prompt}]
            }]
        });

        let resp = reqwest::Client::new()
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Client(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Client(format!("Gemini ask returned {}: {}", status, text)));
        }

        let res_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LlmError::Client(format!("Failed to parse Gemini response: {:?}", e)))?;

        let text = res_json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(text)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct MissingSyncNutrition {
    pub omega_3_dha_mg: f64,
    pub triglycerides_mg: f64,
}

// -------------------------------------------------------------
// Helper to strip markdown block fences if the LLM adds them.

/// Helper to strip markdown block fences if the LLM adds them.
pub fn clean_json_response(text: &str) -> String {
    let mut s = text.trim();
    if s.starts_with("```") {
        s = s.strip_prefix("```").unwrap_or(s);
        if s.starts_with("json") {
            s = s.strip_prefix("json").unwrap_or(s);
        }
    }
    if s.ends_with("```") {
        s = s.strip_suffix("```").unwrap_or(s);
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EmailClassification;

    #[test]
    fn test_clean_json_response() {
        let input1 = "```json\n{\"classification\": \"TRASH\", \"reason\": \"spam\"}\n```";
        assert_eq!(
            clean_json_response(input1),
            "{\"classification\": \"TRASH\", \"reason\": \"spam\"}"
        );

        let input2 = "```\n{\"classification\": \"ARCHIVE\", \"reason\": \"personal\"}\n```";
        assert_eq!(
            clean_json_response(input2),
            "{\"classification\": \"ARCHIVE\", \"reason\": \"personal\"}"
        );

        let input3 = "  {\"classification\": \"LEDGER_STREAM\", \"reason\": \"receipt\"}  ";
        assert_eq!(
            clean_json_response(input3),
            "{\"classification\": \"LEDGER_STREAM\", \"reason\": \"receipt\"}"
        );
    }

    #[test]
    fn test_parse_classification() {
        let raw_json =
            "{\"classification\": \"LEDGER_STREAM\", \"reason\": \"chotu transaction log\"}";
        let parsed: Result<OllamaClassificationResponse, _> = serde_json::from_str(raw_json);
        assert!(parsed.is_ok());
        let res = parsed.unwrap();
        assert_eq!(res.classification, EmailClassification::LedgerStream);
        assert_eq!(res.reason, "chotu transaction log");

        // Verify other classifications
        let categories = vec![
            ("ACTION_ITEM", EmailClassification::ActionItem),
            ("TRAVEL_ITINERARY", EmailClassification::TravelItinerary),
            ("FINANCIAL_BILL", EmailClassification::FinancialBill),
            ("STATEMENT_DOCUMENT", EmailClassification::StatementDocument),
            ("NEWSLETTER", EmailClassification::Newsletter),
            ("PERSONAL_REFERENCE", EmailClassification::PersonalReference),
        ];
        for (name, expected) in categories {
            let json = format!("{{\"classification\": \"{}\", \"reason\": \"test\"}}", name);
            let parsed: OllamaClassificationResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.classification, expected);
        }
    }

    #[test]
    fn test_parse_food_photo_analysis() {
        let json = r#"{
            "kind": "BARCODE",
            "barcode": "737628064502",
            "description": "Thai peanut noodle kit",
            "nutrition": {
                "total_calories": 200,
                "protein_grams": 5.0,
                "carbs_grams": 37.0,
                "fats_grams": 4.0,
                "dominant_macro": "carbs",
                "reasoning": "package photo",
                "omega_3_dha_mg": 0.0,
                "cholesterol_mg": 0.0,
                "saturated_fat_g": 1.0,
                "unsaturated_fat_g": 3.0,
                "triglycerides_mg": 0.0,
                "iron_mg": 0.5,
                "vitamin_b_mg": 0.0,
                "vitamin_c_mg": 0.0,
                "sugar_g": 7.0,
                "fiber_g": 1.0,
                "sodium_mg": 150.0,
                "potassium_mg": 0.0,
                "calcium_mg": 20.0,
                "magnesium_mg": 0.0,
                "zinc_mg": 0.0,
                "vitamin_a_mcg": 0.0,
                "vitamin_d_mcg": 0.0,
                "vitamin_e_mg": 0.0,
                "vitamin_k_mcg": 0.0,
                "caffeine_mg": 0.0,
                "trans_fat_g": 0.0
            }
        }"#;
        let parsed: FoodPhotoAnalysis = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.kind, FoodPhotoKind::Barcode);
        assert_eq!(parsed.barcode.as_deref(), Some("737628064502"));
        assert_eq!(parsed.nutrition.total_calories, 200);
    }

    #[test]
    fn test_parse_intent_classification_samples() {
        let status: IntentClassification = serde_json::from_str(
            r#"{"intent":"STATUS","reason":"asks for today overview"}"#,
        )
        .unwrap();
        assert_eq!(status.into_user_intent(), UserIntent::Status);

        let brief: IntentClassification = serde_json::from_str(
            r#"{"intent":"BRIEF","reason":"morning digest"}"#,
        )
        .unwrap();
        assert_eq!(brief.into_user_intent(), UserIntent::Brief);

        let calendar: IntentClassification = serde_json::from_str(
            r#"{"intent":"CALENDAR","calendar_window":"tomorrow","reason":"tomorrow agenda"}"#,
        )
        .unwrap();
        assert_eq!(
            calendar.into_user_intent(),
            UserIntent::Calendar {
                window: "tomorrow".to_string()
            }
        );

        let calendar_week: IntentClassification = serde_json::from_str(
            r#"{"intent":"CALENDAR","calendar_window":"week","reason":"week view"}"#,
        )
        .unwrap();
        assert_eq!(
            calendar_week.into_user_intent(),
            UserIntent::Calendar {
                window: "week".to_string()
            }
        );

        let calendar_default: IntentClassification = serde_json::from_str(
            r#"{"intent":"CALENDAR","reason":"what's on today"}"#,
        )
        .unwrap();
        assert_eq!(
            calendar_default.into_user_intent(),
            UserIntent::Calendar {
                window: "today".to_string()
            }
        );

        let trends: IntentClassification = serde_json::from_str(
            r#"{"intent":"TRENDS","days":14,"reason":"two week trends"}"#,
        )
        .unwrap();
        assert_eq!(
            trends.into_user_intent(),
            UserIntent::Trends { days: Some(14) }
        );

        let food: IntentClassification = serde_json::from_str(
            r#"{"intent":"FOOD","member_id":"alex","food_description":"2 eggs and toast","reason":"meal log"}"#,
        )
        .unwrap();
        assert_eq!(
            food.into_user_intent(),
            UserIntent::Food {
                member_id: Some("alex".to_string()),
                description: "2 eggs and toast".to_string(),
                date: None,
                time: None,
            }
        );

        let food_yesterday: IntentClassification = serde_json::from_str(
            r#"{"intent":"FOOD","food_description":"pasta and salad","food_date":"2026-08-07","food_time":"19:00","reason":"backdated meal"}"#,
        )
        .unwrap();
        assert_eq!(
            food_yesterday.into_user_intent(),
            UserIntent::Food {
                member_id: None,
                description: "pasta and salad".to_string(),
                date: Some("2026-08-07".to_string()),
                time: Some("19:00".to_string()),
            }
        );

        let food_missing: IntentClassification = serde_json::from_str(
            r#"{"intent":"FOOD","food_description":"","clarify_question":"What did you eat?","reason":"no meal"}"#,
        )
        .unwrap();
        match food_missing.into_user_intent() {
            UserIntent::Unknown { clarify_question } => {
                assert!(clarify_question.to_lowercase().contains("eat"));
            }
            other => panic!("expected Unknown, got {:?}", other),
        }

        let tasks: IntentClassification = serde_json::from_str(
            r#"{"intent":"TASKS","tasks_args":"open","reason":"list open tasks"}"#,
        )
        .unwrap();
        assert_eq!(
            tasks.into_user_intent(),
            UserIntent::Tasks {
                filter: "open".to_string()
            }
        );

        let task_add: IntentClassification = serde_json::from_str(
            r#"{"intent":"TASK_ADD","task_title":"call the dentist","due_raw":"tomorrow 3pm","member_id":"praj","reason":"reminder"}"#,
        )
        .unwrap();
        assert_eq!(
            task_add.into_user_intent(),
            UserIntent::TaskAdd {
                member_id: Some("praj".to_string()),
                title: "call the dentist".to_string(),
                due_raw: Some("tomorrow 3pm".to_string()),
            }
        );

        let task_add_missing: IntentClassification = serde_json::from_str(
            r#"{"intent":"TASK_ADD","task_title":"","clarify_question":"What task?","reason":"empty"}"#,
        )
        .unwrap();
        match task_add_missing.into_user_intent() {
            UserIntent::Unknown { clarify_question } => {
                assert!(clarify_question.to_lowercase().contains("task"));
            }
            other => panic!("expected Unknown, got {:?}", other),
        }

        let memory: IntentClassification = serde_json::from_str(
            r#"{"intent":"MEMORY","memory_query":"what was that Thai curry recipe","reason":"recall note"}"#,
        )
        .unwrap();
        assert_eq!(
            memory.into_user_intent(),
            UserIntent::Memory {
                query: "what was that Thai curry recipe".to_string(),
            }
        );

        let monthly: IntentClassification = serde_json::from_str(
            r#"{"intent":"MONTHLY","month":"2026-07","reason":"July spend"}"#,
        )
        .unwrap();
        assert_eq!(
            monthly.into_user_intent(),
            UserIntent::Monthly {
                yyyy_mm: Some("2026-07".to_string())
            }
        );

        let budget: IntentClassification = serde_json::from_str(
            r#"{"intent":"BUDGET","reason":"food budget progress"}"#,
        )
        .unwrap();
        assert_eq!(budget.into_user_intent(), UserIntent::Budget);

        let plan: IntentClassification = serde_json::from_str(
            r#"{"intent":"PLAN","reason":"show training plan"}"#,
        )
        .unwrap();
        assert_eq!(
            plan.into_user_intent(),
            UserIntent::Plan { regenerate: false }
        );

        let plan_new: IntentClassification = serde_json::from_str(
            r#"{"intent":"PLAN","plan_regenerate":true,"reason":"redo week"}"#,
        )
        .unwrap();
        assert_eq!(
            plan_new.into_user_intent(),
            UserIntent::Plan { regenerate: true }
        );

        let unknown: IntentClassification = serde_json::from_str(
            r#"{"intent":"UNKNOWN","clarify_question":"Did you mean status or tasks?","reason":"ambiguous"}"#,
        )
        .unwrap();
        assert_eq!(
            unknown.into_user_intent(),
            UserIntent::Unknown {
                clarify_question: "Did you mean status or tasks?".to_string()
            }
        );
    }

    #[test]
    fn test_parse_new_extractions() {
        // Test ActionItemExtraction
        let action_json = "{\"task_description\": \"Approve draft roadmap\"}";
        let action: ActionItemExtraction = serde_json::from_str(action_json).unwrap();
        assert_eq!(action.task_description, "Approve draft roadmap");

        // Test TravelItineraryExtraction
        let travel_json = r#"{
            "destination": "Paris, France",
            "start_date": "2026-07-01",
            "end_date": "2026-07-10",
            "details": "Booking Ref: AB12CD, Hotel: Le Paris"
        }"#;
        let travel: TravelItineraryExtraction = serde_json::from_str(travel_json).unwrap();
        assert_eq!(travel.destination, "Paris, France");
        assert_eq!(travel.start_date, Some("2026-07-01".to_string()));
        assert_eq!(travel.end_date, Some("2026-07-10".to_string()));
        assert_eq!(travel.details, "Booking Ref: AB12CD, Hotel: Le Paris");

        // Test UpcomingBillExtraction
        let bill_json = r#"{
            "biller": "Rogers Wireless",
            "amount": 85.50,
            "due_date": "2026-06-25"
        }"#;
        let bill: UpcomingBillExtraction = serde_json::from_str(bill_json).unwrap();
        assert_eq!(bill.biller, "Rogers Wireless");
        assert_eq!(bill.amount, Some(85.50));
        assert_eq!(bill.due_date, Some("2026-06-25".to_string()));

        // Test PersonalReferenceExtraction
        let ref_json = r#"{
            "title": "Chef John's lasagna recipe",
            "url": "https://example.com/lasagna",
            "notes": "Remember to use fresh mozzarella and double the basil."
        }"#;
        let reference: PersonalReferenceExtraction = serde_json::from_str(ref_json).unwrap();
        assert_eq!(reference.title, "Chef John's lasagna recipe");
        assert_eq!(reference.url, Some("https://example.com/lasagna".to_string()));
        assert_eq!(reference.notes, "Remember to use fresh mozzarella and double the basil.");
    }

    #[test]
    fn test_chotu_llm_prompt_path_config() {
        let llm = ChotuLlm::new("http://localhost", 11434, "test-model");
        assert!(llm.prompt_path.is_none());

        let llm = llm.with_prompt_path(Some("prompts/email_classifier_system_prompt.txt".to_string()));
        assert_eq!(llm.prompt_path, Some("prompts/email_classifier_system_prompt.txt".to_string()));
    }
}

