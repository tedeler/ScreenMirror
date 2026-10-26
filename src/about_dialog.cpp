// Qt Widgets dialog for Über. ScreenMirror spawns this as its own process
// and keeps mirroring while it is open. No painting of chrome or glyphs.

#include <QApplication>
#include <QDialog>
#include <QDialogButtonBox>
#include <QEvent>
#include <QFont>
#include <QFontMetrics>
#include <QFormLayout>
#include <QLabel>
#include <QPushButton>
#include <QVBoxLayout>

namespace {

class CloseOnEscape : public QObject {
public:
    explicit CloseOnEscape(QDialog *dialog) : QObject(dialog), dialog(dialog) {}

protected:
    bool eventFilter(QObject *, QEvent *event) override {
        if (event->type() == QEvent::Close) {
            dialog->reject();
        }
        return false;
    }

private:
    QDialog *dialog;
};

QLabel *value(const QString &text) {
    auto *label = new QLabel(text);
    label->setTextInteractionFlags(Qt::TextSelectableByMouse);
    label->setWordWrap(false);
    return label;
}

} // namespace

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    if (argc != 6) {
        return 2;
    }
    const QString title = QString::fromUtf8(argv[1]);
    const QString author = QString::fromUtf8(argv[2]);
    const QString version = QString::fromUtf8(argv[3]);
    const QString git = QString::fromUtf8(argv[4]);
    const QString date = QString::fromUtf8(argv[5]);

    QDialog dialog;
    dialog.setWindowTitle(title);
    dialog.setWindowFlag(Qt::Window);
    dialog.installEventFilter(new CloseOnEscape(&dialog));

    auto *heading = new QLabel(QStringLiteral("ScreenMirror"));
    QFont headingFont = heading->font();
    headingFont.setBold(true);
    headingFont.setPointSize(headingFont.pointSize() + 4);
    heading->setFont(headingFont);

    auto *form = new QFormLayout;
    form->setFieldGrowthPolicy(QFormLayout::ExpandingFieldsGrow);
    form->addRow(QStringLiteral("Autor:"), value(author));
    form->addRow(QStringLiteral("Version:"), value(version));
    form->addRow(QStringLiteral("Git-ID:"), value(git));
    form->addRow(QStringLiteral("Baudatum:"), value(date));

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Close);
    if (auto *close = buttons->button(QDialogButtonBox::Close)) {
        close->setText(QStringLiteral("Schließen"));
    }
    QObject::connect(buttons, &QDialogButtonBox::rejected, &dialog, &QDialog::reject);

    auto *layout = new QVBoxLayout(&dialog);
    layout->addWidget(heading);
    layout->addLayout(form);
    layout->addWidget(buttons);

    const int gitWidth = QFontMetrics(dialog.font()).horizontalAdvance(git);
    dialog.setMinimumWidth(qMax(420, gitWidth + 180));
    dialog.exec();
    return 0;
}
