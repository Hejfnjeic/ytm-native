### App interface.

status-upload-complete = اكتمل الرفع ({ $count }). قد يستغرق YouTube Music دقيقة لمعالجة المقاطع الصوتية.
status-upload-failed = فشل الرفع: { $details }
status-upload-partial = تم الرفع: { $count }. فشل: { $details }
status-waiting-transfer = بانتظار انتهاء النقل للإضافة إلى الموسيقى التي أعجبتك…
status-liking-progress = جارٍ إضافة الإعجابات: { $done } من { $total }…
status-unliking-progress = جارٍ إزالة الإعجابات: { $done } من { $total }…
status-playlist-created = تم إنشاء قائمة التشغيل
status-new-playlist-tracks = المقاطع المضافة إلى قائمة التشغيل الجديدة: { $count }
status-playlist-added = تمت الإضافة إلى قائمة التشغيل
status-playlist-updated = تم تحديث قائمة التشغيل
status-playlist-sorted = تم فرز قائمة التشغيل
status-playlist-sort-failed = تعذّر فرز قائمة التشغيل
status-playlist-move-failed = لم يقبل YouTube Music هذا النقل
status-library-saved = تم الحفظ في المكتبة
status-library-removed = تمت الإزالة من المكتبة
status-unsubscribed = تم إلغاء الاشتراك
status-subscribe-failed = تعذّر الاشتراك
status-unsubscribe-failed = تعذّر إلغاء الاشتراك
status-like-success = الإعجابات المضافة: { $count }
status-like-refused = لم يقبل YouTube Music هذا التغيير. حاول مرة أخرى بعد قليل.
status-refused-count = رفض YouTube Music: { $count }
status-removed-liked = تمت الإزالة من الأغاني التي أعجبتك
status-play-premium = يتطلب “{ $title }” اشتراك YouTube Music Premium
status-play-sign-in = سجّل الدخول لتشغيل “{ $title }”
status-age-restricted = المحتوى مقيّد بفئة عمرية
status-songs = { $count ->
    [zero] { $count } أغنية
    [one] أغنية واحدة
    [two] أغنيتان
    [few] { $count } أغانٍ
    [many] { $count } أغنية
   *[other] { $count } أغنية
}
status-episodes = { $count ->
    [zero] { $count } حلقة
    [one] حلقة واحدة
    [two] حلقتان
    [few] { $count } حلقات
    [many] { $count } حلقة
   *[other] { $count } حلقة
}
